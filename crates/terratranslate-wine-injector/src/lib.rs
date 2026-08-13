use std::fmt;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeArchitecture {
    X86,
    X86_64,
    Arm64,
    Other(u16),
}

impl fmt::Display for PeArchitecture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::X86 => formatter.write_str("x86"),
            Self::X86_64 => formatter.write_str("x86_64"),
            Self::Arm64 => formatter.write_str("arm64"),
            Self::Other(machine) => write!(formatter, "pe-machine-0x{machine:04x}"),
        }
    }
}

pub fn read_pe_architecture(path: &Path) -> Result<PeArchitecture, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    parse_pe_architecture(&bytes)
}

pub fn parse_pe_architecture(bytes: &[u8]) -> Result<PeArchitecture, String> {
    if bytes.get(..2) != Some(b"MZ") {
        return Err("file does not have a DOS MZ header".into());
    }
    let pe_offset = bytes
        .get(0x3c..0x40)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| "DOS header is truncated".to_owned())? as usize;
    if bytes.get(pe_offset..pe_offset.saturating_add(4)) != Some(b"PE\0\0") {
        return Err("file does not have a PE signature".into());
    }
    let machine = bytes
        .get(pe_offset + 4..pe_offset.saturating_add(6))
        .and_then(|value| value.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| "PE COFF header is truncated".to_owned())?;
    Ok(match machine {
        0x014c => PeArchitecture::X86,
        0x8664 => PeArchitecture::X86_64,
        0xaa64 => PeArchitecture::Arm64,
        other => PeArchitecture::Other(other),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(machine: u16) -> Vec<u8> {
        let mut bytes = vec![0; 0x86];
        bytes[0..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&0x80_u32.to_le_bytes());
        bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
        bytes[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
        bytes
    }

    #[test]
    fn parses_both_supported_wine_architectures() {
        assert_eq!(
            parse_pe_architecture(&fixture(0x014c)),
            Ok(PeArchitecture::X86)
        );
        assert_eq!(
            parse_pe_architecture(&fixture(0x8664)),
            Ok(PeArchitecture::X86_64)
        );
    }

    #[test]
    fn rejects_non_pe_input() {
        assert!(parse_pe_architecture(b"not a DLL").is_err());
    }
}
