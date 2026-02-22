//! Simple file path parsing to replace the url crate.
//!
//! This module provides minimal URL/file path parsing functionality
//! specifically for DjVu document file references.

use std::path::{Path, PathBuf};

/// Parse a file URL or path string into a PathBuf.
///
/// Handles:
/// - `file:///path/to/file` → `/path/to/file`
/// - `file://localhost/path` → `/path`
/// - `/path/to/file` → `/path/to/file`
/// - `relative/path` → `relative/path`
///
/// # Examples
///
/// ```
/// use djvu_encoder::utils::file_path::parse_file_path;
///
/// let path = parse_file_path("file:///home/user/doc.djvu");
/// assert_eq!(path.to_str().unwrap(), "/home/user/doc.djvu");
/// ```
pub fn parse_file_path(s: &str) -> PathBuf {
    let s = s.trim();
    
    // Handle file:// URLs
    if s.starts_with("file://") {
        let path_part = &s[7..]; // Skip "file://"
        
        // Remove "localhost" if present
        let path_part = if path_part.starts_with("localhost/") {
            &path_part[9..] // Skip "localhost"
        } else {
            path_part
        };
        
        // On Windows, handle file:///C:/path format
        #[cfg(windows)]
        {
            if path_part.starts_with('/') && path_part.len() > 2 {
                // Check for /C:/ pattern
                let bytes = path_part.as_bytes();
                if bytes.len() > 3 
                    && bytes[1].is_ascii_alphabetic() 
                    && bytes[2] == b':' 
                {
                    return PathBuf::from(&path_part[1..]); // Skip leading /
                }
            }
        }
        
        PathBuf::from(path_part)
    } else {
        // Plain file path
        PathBuf::from(s)
    }
}

/// Convert a file path to a file URL string.
///
/// # Examples
///
/// ```
/// use djvu_encoder::utils::file_path::path_to_file_url;
/// use std::path::Path;
///
/// let url = path_to_file_url(Path::new("/home/user/doc.djvu"));
/// assert_eq!(url, "file:///home/user/doc.djvu");
/// ```
pub fn path_to_file_url(path: &Path) -> String {
    let path_str = path.to_string_lossy();
    
    #[cfg(windows)]
    {
        // Convert backslashes to forward slashes
        let normalized = path_str.replace('\\', "/");
        // Handle Windows absolute paths like C:/path
        if normalized.len() > 2 && normalized.as_bytes()[1] == b':' {
            return format!("file:///{}", normalized);
        }
        format!("file://{}", normalized)
    }
    
    #[cfg(not(windows))]
    {
        if path_str.starts_with('/') {
            format!("file://{}", path_str)
        } else {
            format!("file://{}", path_str)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_file_url() {
        let path = parse_file_path("file:///home/user/test.djvu");
        assert_eq!(path.to_str().unwrap(), "/home/user/test.djvu");
    }

    #[test]
    fn test_parse_file_url_localhost() {
        let path = parse_file_path("file://localhost/home/user/test.djvu");
        assert_eq!(path.to_str().unwrap(), "/home/user/test.djvu");
    }

    #[test]
    fn test_parse_plain_path() {
        let path = parse_file_path("/home/user/test.djvu");
        assert_eq!(path.to_str().unwrap(), "/home/user/test.djvu");
    }

    #[test]
    fn test_parse_relative_path() {
        let path = parse_file_path("relative/test.djvu");
        assert_eq!(path.to_str().unwrap(), "relative/test.djvu");
    }

    #[cfg(windows)]
    #[test]
    fn test_parse_windows_file_url() {
        let path = parse_file_path("file:///C:/Users/test.djvu");
        assert_eq!(path.to_str().unwrap(), "C:/Users/test.djvu");
    }

    #[test]
    fn test_path_to_file_url() {
        let url = path_to_file_url(Path::new("/home/user/test.djvu"));
        assert_eq!(url, "file:///home/user/test.djvu");
    }
}
