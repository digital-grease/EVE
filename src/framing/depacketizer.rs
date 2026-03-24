/// Depacketizer: reassembles [`Frame`]s into a complete file.
use super::{flags, Frame, FramingError};
use std::collections::HashMap;

/// Metadata parsed from the SYN frame payload.
#[derive(Debug)]
struct SynMeta {
    filename: String,
    size: usize,
}

/// Accepts decoded frames (possibly out-of-order) and writes the output file
/// once all frames arrive.
pub struct Depacketizer {
    output_dir: std::path::PathBuf,
    syn_meta: Option<SynMeta>,
    frames: HashMap<u32, Frame>,
    total_data_frames: Option<u32>,
    received_fin: bool,
}

impl Depacketizer {
    /// Create a new depacketizer that writes received files into `output_dir`.
    pub fn new(output_dir: std::path::PathBuf) -> Self {
        Self {
            output_dir,
            syn_meta: None,
            frames: HashMap::new(),
            total_data_frames: None,
            received_fin: false,
        }
    }

    /// Feed a decoded frame into the reassembly engine.
    ///
    /// Returns `Ok(Some(path))` once the file is complete and written to disk,
    /// or `Ok(None)` if more frames are still expected.
    pub fn push(&mut self, frame: Frame) -> Result<Option<std::path::PathBuf>, FramingError> {
        // SYN frame — parse metadata.
        if frame.flags & flags::SYN != 0 {
            let json = String::from_utf8_lossy(&frame.payload);
            let meta = parse_syn_json(&json);
            self.syn_meta = Some(meta);
            // If SYN also has FIN (empty file), handle immediately.
            if frame.flags & flags::FIN != 0 {
                self.received_fin = true;
                self.total_data_frames = Some(0);
            }
        } else {
            // Data frame.
            if frame.flags & flags::FIN != 0 {
                self.received_fin = true;
                // seq is 1-based relative to SYN (seq 0), so the number of
                // data frames = frame.seq (which is the last seq number, 0-based,
                // minus 0 for the SYN frame → frame.seq data frames total).
                self.total_data_frames = Some(frame.seq + 1 - 1); // seq of last data frame
            }
            self.frames.insert(frame.seq, frame);
        }

        self.try_assemble()
    }

    /// Current progress as (received_data_frames, total_data_frames).
    pub fn progress(&self) -> (usize, Option<usize>) {
        (
            self.frames.len(),
            self.total_data_frames.map(|n| n as usize),
        )
    }

    // -----------------------------------------------------------------------

    fn try_assemble(&mut self) -> Result<Option<std::path::PathBuf>, FramingError> {
        let total = match self.total_data_frames {
            Some(t) => t,
            None => return Ok(None),
        };
        let meta = match &self.syn_meta {
            Some(m) => m,
            None => return Ok(None),
        };

        // Check for missing sequences.
        // Data frames occupy seq 1..=total (SYN is seq 0).
        let missing: Vec<u32> = (1..=total)
            .filter(|seq| !self.frames.contains_key(seq))
            .collect();

        if !missing.is_empty() {
            return Ok(None); // still waiting
        }

        // Reassemble in sequence order.
        let mut data: Vec<u8> = Vec::new();
        for seq in 1..=total {
            data.extend_from_slice(&self.frames[&seq].payload);
        }

        // Truncate to the declared size (removes symbol-padding if any).
        data.truncate(meta.size);

        // Write file.
        let safe_name = sanitise_filename(&meta.filename);
        let out_path = self.output_dir.join(&safe_name);
        std::fs::write(&out_path, &data)?;

        let (received, _) = self.progress();
        eprintln!(
            "transfer complete: {} bytes → {:?} ({received}/{total} frames)",
            data.len(),
            out_path
        );

        Ok(Some(out_path))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse the SYN JSON payload `{"filename":"...","size":N}`.
///
/// Uses simple substring search to avoid pulling in a JSON crate.
fn parse_syn_json(json: &str) -> SynMeta {
    let filename = extract_json_string(json, "filename").unwrap_or_default();
    let size = extract_json_number(json, "size").unwrap_or(0);
    SynMeta { filename, size }
}

fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\":\"", key);
    let start = json.find(&needle)? + needle.len();
    let rest = &json[start..];
    let mut end = 0;
    let chars: Vec<char> = rest.chars().collect();
    while end < chars.len() {
        if chars[end] == '"' && (end == 0 || chars[end - 1] != '\\') {
            break;
        }
        end += 1;
    }
    Some(rest[..end].replace("\\\"", "\"").replace("\\\\", "\\"))
}

fn extract_json_number(json: &str, key: &str) -> Option<usize> {
    let needle = format!("\"{}\":", key);
    let start = json.find(&needle)? + needle.len();
    let rest = json[start..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Replace path separators and null bytes to prevent directory traversal.
fn sanitise_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == '\0' {
                '_'
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{flags, Frame};

    fn data_frame(seq: u32, payload: &[u8], fin: bool) -> Frame {
        Frame {
            seq,
            flags: if fin { flags::FIN } else { 0 },
            payload: payload.to_vec(),
        }
    }

    fn syn_frame(filename: &str, size: usize) -> Frame {
        Frame {
            seq: 0,
            flags: flags::SYN,
            payload: format!(r#"{{"filename":"{}","size":{}}}"#, filename, size).into_bytes(),
        }
    }

    #[test]
    fn test_in_order_assembly() {
        let dir = tempdir();
        let mut d = Depacketizer::new(dir.clone());
        d.push(syn_frame("out.bin", 6)).unwrap();
        d.push(data_frame(1, b"abc", false)).unwrap();
        let result = d.push(data_frame(2, b"def", true)).unwrap();
        assert!(result.is_some());
        let path = result.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"abcdef");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_out_of_order_assembly() {
        let dir = tempdir();
        let mut d = Depacketizer::new(dir.clone());
        d.push(syn_frame("oo.bin", 6)).unwrap();
        d.push(data_frame(2, b"def", true)).unwrap(); // arrives first
        let result = d.push(data_frame(1, b"abc", false)).unwrap();
        assert!(result.is_some());
        let path = result.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"abcdef");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_incomplete_returns_none() {
        let dir = tempdir();
        let mut d = Depacketizer::new(dir.clone());
        d.push(syn_frame("inc.bin", 6)).unwrap();
        let result = d.push(data_frame(1, b"abc", false)).unwrap();
        assert!(result.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_empty_file() {
        let dir = tempdir();
        let mut d = Depacketizer::new(dir.clone());
        let mut syn = syn_frame("empty.bin", 0);
        syn.flags |= flags::FIN; // empty file: SYN+FIN
        let result = d.push(syn).unwrap();
        assert!(result.is_some());
        let path = result.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Create a unique temporary directory for test output.
    fn tempdir() -> std::path::PathBuf {
        // Use both process ID and thread ID to avoid collisions across parallel tests.
        let thread_id = format!("{:?}", std::thread::current().id());
        let path = std::env::temp_dir().join(format!(
            "eve_test_{}_{}",
            std::process::id(),
            thread_id.replace(['(', ')'], "")
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}
