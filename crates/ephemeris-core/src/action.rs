/// Application action triggered by input or commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Undo,
    Redo,
    Clear,
    Menu,
    Save,
    SelectTool(Tool),
    ChangeColor,
}

use crate::model::Tool;

/// Map input events to application actions.
pub trait ActionMap {
    fn event_to_action(&self, button_id: u32, pressed: bool) -> Option<Action>;
}

/// Default action mapping for pen buttons.
pub struct DefaultActionMap;

impl DefaultActionMap {
    pub fn new() -> Self {
        DefaultActionMap
    }
}

impl Default for DefaultActionMap {
    fn default() -> Self {
        DefaultActionMap::new()
    }
}

impl ActionMap for DefaultActionMap {
    fn event_to_action(&self, button_id: u32, pressed: bool) -> Option<Action> {
        if !pressed {
            return None;
        }
        match button_id {
            1 => Some(Action::Undo),
            2 => Some(Action::Redo),
            3 => Some(Action::Clear),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_mapping_button_1() {
        let map = DefaultActionMap::new();
        assert_eq!(map.event_to_action(1, true), Some(Action::Undo));
    }

    #[test]
    fn action_mapping_button_2() {
        let map = DefaultActionMap::new();
        assert_eq!(map.event_to_action(2, true), Some(Action::Redo));
    }

    #[test]
    fn action_mapping_button_3() {
        let map = DefaultActionMap::new();
        assert_eq!(map.event_to_action(3, true), Some(Action::Clear));
    }

    #[test]
    fn action_mapping_button_release_no_action() {
        let map = DefaultActionMap::new();
        assert_eq!(map.event_to_action(1, false), None);
    }

    #[test]
    fn action_mapping_unknown_button() {
        let map = DefaultActionMap::new();
        assert_eq!(map.event_to_action(99, true), None);
    }
}
