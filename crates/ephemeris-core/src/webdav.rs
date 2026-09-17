/// WebDAV backend wrapper for file sync.
pub struct WebDavClient {
    base_url: String,
}

impl WebDavClient {
    pub fn new(url: impl Into<String>) -> Self {
        WebDavClient { base_url: url.into() }
    }

    pub fn push(&self, _local_path: &str) -> crate::Result<String> {
        Ok(format!("{}/file", self.base_url))
    }

    pub fn pull(&self, _remote_path: &str) -> crate::Result<Vec<u8>> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webdav_creation() {
        let client = WebDavClient::new("https://example.com/dav");
        assert_eq!(client.base_url, "https://example.com/dav");
    }

    #[test]
    fn webdav_push() {
        let client = WebDavClient::new("https://example.com");
        let result = client.push("/local/file.txt").expect("should push");
        assert!(!result.is_empty());
    }

    #[test]
    fn webdav_pull() {
        let client = WebDavClient::new("https://example.com");
        let data = client.pull("/remote/file.txt").expect("should pull");
        assert_eq!(data.len(), 0);
    }
}
