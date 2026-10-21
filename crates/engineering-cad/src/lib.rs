//! CAD integration surface.
//!
//! Format detection is real and bounded (header sniffing plus the binary STL
//! size invariant). No BREP/mesh parser is implemented yet, so import always
//! returns a typed [`CadImportOutcome::Unavailable`] naming the parser that is
//! missing. It never returns an empty or invented geometry.

use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_HEADER_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CadFormat {
    Step,
    Iges,
    Dxf,
    StlAscii,
    StlBinary,
    Unknown,
}

impl CadFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Step => "step",
            Self::Iges => "iges",
            Self::Dxf => "dxf",
            Self::StlAscii => "stl_ascii",
            Self::StlBinary => "stl_binary",
            Self::Unknown => "unknown",
        }
    }

    pub const fn parser_required(self) -> Option<&'static str> {
        match self {
            Self::Step => Some("ISO 10303-21 (STEP) BREP reader"),
            Self::Iges => Some("IGES BREP reader"),
            Self::Dxf => Some("DXF entity reader"),
            Self::StlAscii | Self::StlBinary => Some("triangle mesh reader with unit metadata"),
            Self::Unknown => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum CadError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("metadata for {path} is not a regular file")]
    NotAFile { path: String },
}

fn io_error(path: &Path, source: std::io::Error) -> CadError {
    CadError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn read_header(path: &Path) -> Result<Vec<u8>, CadError> {
    let file = std::fs::File::open(path).map_err(|error| io_error(path, error))?;
    let metadata = file.metadata().map_err(|error| io_error(path, error))?;
    if !metadata.is_file() {
        return Err(CadError::NotAFile {
            path: path.display().to_string(),
        });
    }
    let mut header = Vec::new();
    file.take(MAX_HEADER_BYTES as u64)
        .read_to_end(&mut header)
        .map_err(|error| io_error(path, error))?;
    Ok(header)
}

fn trimmed_start(header: &[u8]) -> &[u8] {
    let start = if header.starts_with(&[0xEF, 0xBB, 0xBF]) {
        3
    } else {
        0
    };
    let mut index = start;
    while index < header.len() && header[index].is_ascii_whitespace() {
        index += 1;
    }
    &header[index..]
}

/// Detect a CAD container. Detection is not import: it only classifies the
/// bytes and extension so an honest refusal can name the missing parser.
pub fn detect_format(path: &Path) -> Result<CadFormat, CadError> {
    let header = read_header(path)?;
    let metadata = std::fs::metadata(path).map_err(|error| io_error(path, error))?;
    let start = trimmed_start(&header);
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if start.starts_with(b"ISO-10303-21;") {
        return Ok(CadFormat::Step);
    }
    if matches!(extension.as_str(), "igs" | "iges") {
        let looks_iges = header.contains(&b'S') && header.iter().all(|byte| byte.is_ascii());
        if looks_iges {
            return Ok(CadFormat::Iges);
        }
    }
    if start.starts_with(b"0\nSECTION") || start.starts_with(b"0\r\nSECTION") {
        return Ok(CadFormat::Dxf);
    }
    if start.starts_with(b"solid ")
        && header
            .windows(6)
            .any(|window| window.eq_ignore_ascii_case(b"facet "))
    {
        return Ok(CadFormat::StlAscii);
    }
    if extension == "stl" && header.len() >= 84 {
        let count = u32::from_le_bytes([header[80], header[81], header[82], header[83]]);
        let expected = 84u64 + u64::from(count) * 50;
        if metadata.len() == expected {
            return Ok(CadFormat::StlBinary);
        }
    }
    Ok(CadFormat::Unknown)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CadImportOutcome {
    /// The declared surface. There is no `Imported` variant yet because no
    /// real format parser is implemented; adding geometry without a parser
    /// would be fabricated data.
    Unavailable {
        format: CadFormat,
        reason: String,
        parser_required: Option<String>,
        file_bytes: u64,
    },
}

impl CadImportOutcome {
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }
}

/// Import a CAD file. Detection is real; import is a typed unavailable until
/// a parser exists. The returned reason names both the format and the missing
/// parser, so callers can route to a future implementation or ask the user.
pub fn import(path: &Path) -> Result<CadImportOutcome, CadError> {
    import_cad(path)
}

pub fn import_cad(path: &Path) -> Result<CadImportOutcome, CadError> {
    let format = detect_format(path)?;
    let file_bytes = std::fs::metadata(path)
        .map_err(|error| io_error(path, error))?
        .len();
    let reason = match format {
        CadFormat::Unknown => {
            "the CAD container format was not recognized; no geometry was imported".to_string()
        }
        other => format!(
            "no {} parser is implemented in this build; no geometry was imported",
            other.as_str()
        ),
    };
    Ok(CadImportOutcome::Unavailable {
        format,
        reason,
        parser_required: format.parser_required().map(str::to_string),
        file_bytes,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn write_temp(name: &str, bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn step_is_detected_but_refused_as_unavailable() {
        let (_dir, path) = write_temp(
            "part.step",
            b"ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION(('x'),'2;1');\nENDSEC;\n",
        );
        assert_eq!(detect_format(&path).unwrap(), CadFormat::Step);
        let outcome = import_cad(&path).unwrap();
        assert!(outcome.is_unavailable());
        match outcome {
            CadImportOutcome::Unavailable {
                format,
                parser_required,
                ..
            } => {
                assert_eq!(format, CadFormat::Step);
                assert!(parser_required.unwrap().contains("STEP"));
            }
        }
    }

    #[test]
    fn ascii_stl_requires_a_real_facet_block() {
        let (_dir, path) = write_temp(
            "part.stl",
            b"solid part\n facet normal 0 0 1\n  outer loop\n  endloop\n endfacet\nendsolid part\n",
        );
        assert_eq!(detect_format(&path).unwrap(), CadFormat::StlAscii);
        let (dir, path) = write_temp(
            "not_mesh.stl",
            b"solid but only a name\nno triangles here\n",
        );
        assert_eq!(detect_format(&path).unwrap(), CadFormat::Unknown);
        drop(dir);
    }

    #[test]
    fn binary_stl_uses_the_size_invariant() {
        let mut bytes = vec![0u8; 84];
        bytes[..12].copy_from_slice(b"binary stl!!");
        bytes[80..84].copy_from_slice(&2u32.to_le_bytes());
        bytes.extend(std::iter::repeat_n(0u8, 2 * 50));
        let (_dir, path) = write_temp("part.stl", &bytes);
        assert_eq!(detect_format(&path).unwrap(), CadFormat::StlBinary);

        let mut truncated = bytes.clone();
        truncated.truncate(bytes.len() - 1);
        let (_dir2, path2) = write_temp("bad.stl", &truncated);
        assert_eq!(detect_format(&path2).unwrap(), CadFormat::Unknown);
    }

    #[test]
    fn dxf_and_iges_detection_is_bounded() {
        let (_dir, dxf) = write_temp("drawing.dxf", b"0\nSECTION\n2\nENTITIES\n0\nENDSEC\n");
        assert_eq!(detect_format(&dxf).unwrap(), CadFormat::Dxf);
        let (_dir2, iges) = write_temp(
            "surface.iges",
            b"                                                                        S      1\n",
        );
        assert_eq!(detect_format(&iges).unwrap(), CadFormat::Iges);
    }

    #[test]
    fn missing_and_directory_paths_are_typed_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_format(&dir.path().join("missing.step")).is_err());
        assert!(matches!(
            detect_format(dir.path()),
            Err(CadError::NotAFile { .. })
        ));
    }
}
