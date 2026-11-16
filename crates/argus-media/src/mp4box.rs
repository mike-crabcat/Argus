/// Appends an ISO BMFF box, back-patching its 32-bit size.
pub(crate) fn write_box(buf: &mut Vec<u8>, fourcc: &[u8; 4], body: impl FnOnce(&mut Vec<u8>)) {
    let start = buf.len();
    buf.extend_from_slice(&[0; 4]);
    buf.extend_from_slice(fourcc);
    body(buf);
    let len = u32::try_from(buf.len() - start).expect("box larger than 4 GiB");
    buf[start..start + 4].copy_from_slice(&len.to_be_bytes());
}

/// Appends a "full box" (a box with a version byte and 24-bit flags).
pub(crate) fn write_full_box(
    buf: &mut Vec<u8>,
    fourcc: &[u8; 4],
    version: u8,
    flags: u32,
    body: impl FnOnce(&mut Vec<u8>),
) {
    write_box(buf, fourcc, |b| {
        b.push(version);
        b.extend_from_slice(&flags.to_be_bytes()[1..]);
        body(b);
    });
}

pub(crate) const UNITY_MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

pub(crate) fn put_matrix(b: &mut Vec<u8>) {
    for v in UNITY_MATRIX {
        b.extend_from_slice(&v.to_be_bytes());
    }
}
