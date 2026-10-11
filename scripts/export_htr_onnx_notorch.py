#!/usr/bin/env python3
"""Export OnlineHTR ckpt → ONNX without importing torch (broken OpenMP on this host)."""
from __future__ import annotations

import io
import json
import os
import pickle
import struct
import zipfile
from pathlib import Path

import numpy as np

try:
    import onnx
    from onnx import TensorProto, helper, numpy_helper
except ImportError as e:
    raise SystemExit("Need onnx: uv pip install --python .venv12/bin/python onnx") from e

CKPT_DIR = Path(os.environ["HTR_CKPT_DIR"]).resolve()
OUT_DIR = Path(os.environ["HTR_OUT_DIR"]).resolve()
ALPHABET_JSON = Path(os.environ["HTR_ALPHABET"]).resolve()
OUT_DIR.mkdir(parents=True, exist_ok=True)

DTYPE_MAP = {
    "FloatStorage": np.float32,
    "HalfStorage": np.float16,
    "DoubleStorage": np.float64,
    "LongStorage": np.int64,
    "IntStorage": np.int32,
    "ByteStorage": np.uint8,
    "BoolStorage": np.bool_,
    "BFloat16Storage": np.float16,  # approximate
}


class _Tensor:
    def __init__(self, data: np.ndarray):
        self.data = data

    def numpy(self):
        return self.data

    @property
    def shape(self):
        return self.data.shape


def load_ckpt_state(ckpt_path: Path) -> tuple[dict[str, np.ndarray], dict]:
    """Load state_dict + hyper_parameters from a Lightning .ckpt zip without torch."""
    zf = zipfile.ZipFile(ckpt_path)
    # Find data.pkl (PyTorch 1.6+ zip format)
    pkl_name = None
    for n in zf.namelist():
        if n.endswith("data.pkl"):
            pkl_name = n
            break
    if pkl_name is None:
        raise SystemExit(f"No data.pkl in {ckpt_path}")

    prefix = pkl_name[: -len("data.pkl")]
    storages: dict[str, np.ndarray] = {}

    class Unpickler(pickle.Unpickler):
        def persistent_load(self, saved_id):
            if isinstance(saved_id, tuple) and saved_id[0] == "storage":
                # ('storage', storage_type, key, location, numel)
                storage_type, key, _location, numel = saved_id[1:5]
                key = str(key)
                if key not in storages:
                    raw = zf.read(f"{prefix}data/{key}")
                    type_name = getattr(storage_type, "__name__", str(storage_type))
                    # storage_type may be a class we faked
                    if isinstance(storage_type, type):
                        type_name = storage_type.__name__
                    dtype = DTYPE_MAP.get(type_name, np.float32)
                    arr = np.frombuffer(raw, dtype=dtype, count=int(numel)).copy()
                    storages[key] = arr
                return storages[key]
            raise pickle.UnpicklingError(f"Unknown persistent id {saved_id!r}")

        def find_class(self, module, name):
            if module.startswith("torch"):
                if name.endswith("Storage") or name in DTYPE_MAP:

                    class StorageType:
                        __name__ = name

                    StorageType.__name__ = name
                    return StorageType
                if name == "_rebuild_tensor_v2":

                    def _rebuild_tensor_v2(
                        storage, storage_offset, size, stride, requires_grad, backward_hooks, *rest
                    ):
                        size = tuple(int(x) for x in size)
                        stride = tuple(int(x) for x in stride)
                        storage_offset = int(storage_offset)
                        # Materialize contiguous view
                        if len(size) == 0:
                            data = storage[storage_offset : storage_offset + 1].reshape(())
                        else:
                            # General strided → contiguous
                            out = np.empty(size, dtype=storage.dtype)
                            if out.size == 0:
                                return _Tensor(out)
                            # Slow but fine for model weights
                            flat = storage
                            it = np.ndindex(size)
                            for idx in it:
                                off = storage_offset
                                for i, s in zip(idx, stride):
                                    off += i * s
                                out[idx] = flat[off]
                            data = out
                        return _Tensor(data)

                    return _rebuild_tensor_v2
                if name == "_rebuild_parameter":

                    def _rebuild_parameter(data, requires_grad, backward_hooks):
                        return data

                    return _rebuild_parameter
                if name == "Size":
                    return tuple
                if name == "dtype":
                    return type("dtype", (), {})
                # omegaconf / lightning leftovers: return dummy types
                return type(name, (), {"__init__": lambda self, *a, **k: None, "__setstate__": lambda self, s: None})
            if module.startswith("omegaconf") or module.startswith("lightning") or module.startswith("pytorch_lightning"):
                return type(name, (), {"__init__": lambda self, *a, **k: None, "__setstate__": lambda self, s: None})
            try:
                return super().find_class(module, name)
            except (AttributeError, ModuleNotFoundError):
                return type(name, (), {"__init__": lambda self, *a, **k: None, "__setstate__": lambda self, s: None})

    obj = Unpickler(io.BytesIO(zf.read(pkl_name))).load()
    # Lightning checkpoint is a dict
    if not isinstance(obj, dict):
        raise SystemExit(f"Unexpected ckpt root type {type(obj)}")
    state = obj.get("state_dict", obj)
    hp = obj.get("hyper_parameters", {})
    # hyper_parameters may be OmegaConf-like dummy — extract from pickle strings if needed
    out_state: dict[str, np.ndarray] = {}
    for k, v in state.items():
        if isinstance(v, _Tensor):
            out_state[k] = v.data
        elif isinstance(v, np.ndarray):
            out_state[k] = v
    return out_state, hp if isinstance(hp, dict) else {}


def pick_checkpoint(ckpt_dir: Path) -> Path:
    scored = []
    for p in ckpt_dir.glob("*.ckpt"):
        loss = 999.0
        if "val_loss=" in p.name:
            try:
                loss = float(p.name.split("val_loss=")[1].removesuffix(".ckpt"))
            except ValueError:
                pass
        scored.append((loss, p))
    if not scored:
        raise SystemExit(f"No checkpoints in {ckpt_dir}")
    scored.sort(key=lambda x: x[0])
    return scored[0][1]


def _const(name: str, arr: np.ndarray):
    if arr.dtype == np.int64:
        return numpy_helper.from_array(arr, name=name)
    return numpy_helper.from_array(arr.astype(np.float32), name=name)


def build_lstm_onnx(
    weights: dict[str, np.ndarray],
    num_layers: int,
    hidden: int,
    num_classes: int,
    out_path: Path,
):
    """Build bidirectional stacked LSTM + Linear + LogSoftmax ONNX (NTC in/out).

    PyTorch LSTM weight layout per direction:
      weight_ih: [4*H, input_size]
      weight_hh: [4*H, H]
      bias_ih / bias_hh: [4*H]
    Gate order: I F G O  — ONNX LSTM uses I O F G, so we reorder.
    """

    def reorder_ifgo_to_iofg(w: np.ndarray, axis: int = 0) -> np.ndarray:
        # split 4*H along axis into I,F,G,O then stack I,O,F,G
        chunks = np.split(w, 4, axis=axis)
        i, f, g, o = chunks
        return np.concatenate([i, o, f, g], axis=axis)

    initializers = []
    nodes = []
    # Input: features [N, T, 4]
    # ONNX LSTM wants [T, N, C] or with layout — we'll transpose.

    # Transpose NTC → TNC
    nodes.append(
        helper.make_node("Transpose", ["features"], ["x_tnc"], perm=[1, 0, 2])
    )

    cur = "x_tnc"
    cur_size = 4

    for layer in range(num_layers):
        # Forward
        wih_f = weights[f"lstm_stack.weight_ih_l{layer}"]
        whh_f = weights[f"lstm_stack.weight_hh_l{layer}"]
        bih_f = weights[f"lstm_stack.bias_ih_l{layer}"]
        bhh_f = weights[f"lstm_stack.bias_hh_l{layer}"]
        # Reverse
        wih_r = weights[f"lstm_stack.weight_ih_l{layer}_reverse"]
        whh_r = weights[f"lstm_stack.weight_hh_l{layer}_reverse"]
        bih_r = weights[f"lstm_stack.bias_ih_l{layer}_reverse"]
        bhh_r = weights[f"lstm_stack.bias_hh_l{layer}_reverse"]

        # ONNX W: [num_directions, 4*H, input_size]
        W = np.stack(
            [reorder_ifgo_to_iofg(wih_f), reorder_ifgo_to_iofg(wih_r)], axis=0
        ).astype(np.float32)
        # ONNX R: [num_directions, 4*H, H]
        R = np.stack(
            [reorder_ifgo_to_iofg(whh_f), reorder_ifgo_to_iofg(whh_r)], axis=0
        ).astype(np.float32)
        # ONNX B: [num_directions, 8*H] = concat(bias_ih, bias_hh) with gate reorder
        B = np.stack(
            [
                np.concatenate(
                    [reorder_ifgo_to_iofg(bih_f), reorder_ifgo_to_iofg(bhh_f)]
                ),
                np.concatenate(
                    [reorder_ifgo_to_iofg(bih_r), reorder_ifgo_to_iofg(bhh_r)]
                ),
            ],
            axis=0,
        ).astype(np.float32)

        Wn, Rn, Bn = f"W{layer}", f"R{layer}", f"B{layer}"
        initializers.extend([_const(Wn, W), _const(Rn, R), _const(Bn, B)])

        y_name = f"lstm_y{layer}"
        y_h = f"lstm_yh{layer}"
        y_c = f"lstm_yc{layer}"
        nodes.append(
            helper.make_node(
                "LSTM",
                inputs=[cur, Wn, Rn, Bn, "", "", ""],  # empty seq_lens, i_h, i_c
                outputs=[y_name, y_h, y_c],
                name=f"lstm{layer}",
                hidden_size=hidden,
                direction="bidirectional",
            )
        )
        # Y: [T, num_directions, N, H] → concat directions → [T, N, 2H]
        # Use Transpose + Reshape
        # Transpose to [T, N, D, H] then reshape [T, N, D*H]
        yt = f"y_t{layer}"
        nodes.append(
            helper.make_node("Transpose", [y_name], [yt], perm=[0, 2, 1, 3])
        )
        ycat = f"y_cat{layer}"
        # [T,N,D,H] → [T,N,D*H]; 0 keeps T/N from input (allowzero default 0)
        shape_c = f"shape_c{layer}"
        initializers.append(
            _const(shape_c, np.array([0, 0, 2 * hidden], dtype=np.int64))
        )
        nodes.append(helper.make_node("Reshape", [yt, shape_c], [ycat]))
        cur = ycat
        cur_size = 2 * hidden

    # Linear: [2H, C]^T in PyTorch as weight [C, 2H]
    lw = weights["linear.weight"].astype(np.float32)  # [C, 2H]
    lb = weights["linear.bias"].astype(np.float32)  # [C]
    initializers.append(_const("linear_w", lw.T.copy()))  # [2H, C] for MatMul
    initializers.append(_const("linear_b", lb))

    # cur is [T, N, 2H] — MatMul last dim
    nodes.append(helper.make_node("MatMul", [cur, "linear_w"], ["lin"]))
    nodes.append(helper.make_node("Add", ["lin", "linear_b"], ["logits_tnc"]))
    # LogSoftmax on class dim (axis=2)
    nodes.append(
        helper.make_node("LogSoftmax", ["logits_tnc"], ["logp_tnc"], axis=2)
    )
    # Transpose back to NTC
    nodes.append(
        helper.make_node("Transpose", ["logp_tnc"], ["logits"], perm=[1, 0, 2])
    )

    graph = helper.make_graph(
        nodes,
        "online_htr",
        [
            helper.make_tensor_value_info(
                "features", TensorProto.FLOAT, ["batch", "time", 4]
            )
        ],
        [
            helper.make_tensor_value_info(
                "logits", TensorProto.FLOAT, ["batch", "time", num_classes]
            )
        ],
        initializers,
    )
    model = helper.make_model(
        graph,
        opset_imports=[helper.make_opsetid("", 17)],
        producer_name="ephemeris-export-htr",
    )
    model.ir_version = 9
    onnx.checker.check_model(model)
    onnx.save(model, str(out_path))


def main():
    ckpt_path = pick_checkpoint(CKPT_DIR)
    print(f"Loading {ckpt_path} (no torch)")
    weights, hp = load_ckpt_state(ckpt_path)
    print(f"Loaded {len(weights)} tensors; hp keys={list(hp)[:12]}")
    for k, v in sorted(weights.items()):
        if k.startswith("lstm_stack.") or k.startswith("linear."):
            print(f"  {k}: {v.shape} {v.dtype}")

    with open(ALPHABET_JSON) as f:
        alphabet = json.load(f)["alphabet"]
    num_classes = len(alphabet) + 1

    # Infer architecture from weights
    # weight_ih_l0: [4*H, input_size]
    w0 = weights["lstm_stack.weight_ih_l0"]
    hidden = int(w0.shape[0] // 4)
    input_size = int(w0.shape[1])
    num_layers = sum(
        1 for k in weights if k.startswith("lstm_stack.weight_ih_l") and "_reverse" not in k
    )
    print(
        f"Inferred hidden={hidden} layers={num_layers} in={input_size} classes={num_classes}"
    )
    assert input_size == 4
    assert weights["linear.weight"].shape == (num_classes, 2 * hidden)

    onnx_path = OUT_DIR / "model.onnx"
    build_lstm_onnx(weights, num_layers, hidden, num_classes, onnx_path)
    print(f"Wrote {onnx_path} ({onnx_path.stat().st_size} bytes)")

    charset_path = OUT_DIR / "charset.txt"
    with open(charset_path, "w", encoding="utf-8") as f:
        for ch in alphabet:
            f.write(ch + "\n")
    print(f"Wrote {charset_path} ({len(alphabet)} chars)")

    meta = {
        "blank_index": 0,
        "input_layout": "NTC",
        "output_layout": "NTC",
        "features": ["dx", "dy", "dt", "n"],
        "points_per_unit": 20,
        "source": "PellelNitram/OnlineHTR dataIAMOnDB_featuresLinInterpol20DxDyDtN_decoderGreedy",
        "checkpoint": ckpt_path.name,
        "num_classes": num_classes,
        "nodes_per_layer": hidden,
        "number_of_layers": num_layers,
        "exporter": "export_onnx_notorch.py",
    }
    (OUT_DIR / "meta.json").write_text(json.dumps(meta, indent=2) + "\n")
    print("Wrote meta.json")


if __name__ == "__main__":
    main()
