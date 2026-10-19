use std::io::Write;
use std::path::Path;

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            let mask = if crc & 1 == 1 { 0xedb8_8320 } else { 0 };
            crc = (crc >> 1) ^ mask;
        }
    }
    crc ^ 0xffff_ffff
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

pub fn run(args: &[String]) -> i32 {
    if args.len() < 2 {
        eprintln!("usage: faktor-ci-tools make-zip OUT FILE...");
        return 2;
    }
    let out = &args[0];
    let mut entries: Vec<(String, Vec<u8>, u32, u32)> = Vec::new();
    for path in &args[1..] {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("make-zip: {path}: {error}");
                return 1;
            }
        };
        let crc = crc32(&bytes);
        entries.push((path.clone(), bytes, crc, 0));
    }
    let mut output = Vec::new();
    let mut central = Vec::new();
    for (name, bytes, crc, _) in entries.iter_mut() {
        let offset = output.len() as u32;
        put_u32(&mut output, 0x0403_4b50);
        put_u16(&mut output, 20);
        put_u16(&mut output, 0);
        put_u16(&mut output, 0);
        put_u16(&mut output, 0);
        put_u16(&mut output, 0);
        put_u32(&mut output, *crc);
        put_u32(&mut output, bytes.len() as u32);
        put_u32(&mut output, bytes.len() as u32);
        put_u16(&mut output, name.len() as u16);
        put_u16(&mut output, 0);
        output.extend_from_slice(name.as_bytes());
        output.extend_from_slice(bytes);

        put_u32(&mut central, 0x0201_4b50);
        put_u16(&mut central, 20);
        put_u16(&mut central, 20);
        put_u16(&mut central, 0);
        put_u16(&mut central, 0);
        put_u16(&mut central, 0);
        put_u16(&mut central, 0);
        put_u32(&mut central, *crc);
        put_u32(&mut central, bytes.len() as u32);
        put_u32(&mut central, bytes.len() as u32);
        put_u16(&mut central, name.len() as u16);
        put_u16(&mut central, 0);
        put_u16(&mut central, 0);
        put_u16(&mut central, 0);
        put_u16(&mut central, 0);
        put_u32(&mut central, 0);
        put_u32(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = output.len() as u32;
    output.extend_from_slice(&central);
    let central_size = central.len() as u32;
    put_u32(&mut output, 0x0605_4b50);
    put_u16(&mut output, 0);
    put_u16(&mut output, 0);
    put_u16(&mut output, entries.len() as u16);
    put_u16(&mut output, entries.len() as u16);
    put_u32(&mut output, central_size);
    put_u32(&mut output, central_offset);
    put_u16(&mut output, 0);
    match std::fs::File::create(Path::new(out)) {
        Ok(mut file) => match file.write_all(&output) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("make-zip: {out}: {error}");
                1
            }
        },
        Err(error) => {
            eprintln!("make-zip: {out}: {error}");
            1
        }
    }
}
