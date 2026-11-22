//! A small CFB file containing just the streams passed to a legacy decoder.
use office_oxide::cfb::{CFB_SIGNATURE, END_OF_CHAIN, FAT_SECT, FREE_SECT};

const SECTOR: usize = 4096;
const MINI_SECTOR: usize = 64;

pub(super) fn pack(mut streams: Vec<(&str, Vec<u8>)>) -> Result<Vec<u8>, String> {
    if streams.len() >= SECTOR / 128 {
        return Err("compound projection has too many streams".into());
    }
    // CFB directory trees order entries by UTF-16 length, then uppercase name.
    streams.sort_by_key(|(name, _)| (name.encode_utf16().count(), name.to_uppercase()));
    let allocation = streams
        .iter()
        .map(|(_, data)| data.len().next_multiple_of(SECTOR))
        .sum::<usize>();
    let mut file = Vec::with_capacity(allocation + 12 * SECTOR);
    file.resize(2 * SECTOR, 0);
    let mut fat = vec![END_OF_CHAIN]; // Directory sector.
    let mut mini_fat = Vec::new();
    let mut mini_stream = Vec::new();
    for (index, (name, data)) in streams.into_iter().enumerate() {
        let start = if data.len() < SECTOR {
            append_chain(&mut mini_stream, &mut mini_fat, &data, MINI_SECTOR)
        } else {
            append_chain(&mut file, &mut fat, &data, SECTOR)
        };
        let entry = &mut file[SECTOR + (index + 1) * 128..][..128];
        write_entry(entry, name, 2, start, data.len())?;
    }
    let mini_start = append_chain(&mut file, &mut fat, &mini_stream, SECTOR);
    let mini_fat_sectors = (mini_fat.len() * 4).div_ceil(SECTOR);
    mini_fat.resize(mini_fat_sectors * (SECTOR / 4), FREE_SECT);
    let fat_bytes = sectors_as_bytes(&mini_fat);
    let mini_fat_start = append_chain(&mut file, &mut fat, &fat_bytes, SECTOR);

    let entries = &mut file[SECTOR..2 * SECTOR];
    write_entry(
        &mut entries[..128],
        "Root Entry",
        5,
        mini_start,
        mini_stream.len(),
    )?;
    let entry_count = entries
        .chunks_exact(128)
        .skip(1)
        .take_while(|entry| entry[66] != 0)
        .count();
    let max_depth = (entry_count.max(1)).ilog2();
    let child = write_tree(entries, 1, entry_count + 1, 0, max_depth);
    put_u32(entries, 76, child);

    let first_fat = fat.len();
    let fat_sectors = first_fat.div_ceil(SECTOR / 4 - 1);
    if fat_sectors > 109 {
        return Err("compound projection exceeds FAT capacity".into());
    }
    fat.resize(first_fat + fat_sectors, FAT_SECT);
    fat.resize(fat_sectors * (SECTOR / 4), FREE_SECT);
    file.extend_from_slice(&sectors_as_bytes(&fat));

    let header = &mut file[..512];
    header[..8].copy_from_slice(&CFB_SIGNATURE);
    for (offset, value) in [(24, 0x3e), (26, 4), (28, 0xfffe), (30, 12), (32, 6)] {
        header[offset..offset + 2].copy_from_slice(&u16::to_le_bytes(value));
    }
    for (offset, value) in [
        (40, 1),
        (44, fat_sectors as u32),
        (48, 0),
        (56, SECTOR as u32),
        (60, mini_fat_start),
        (64, mini_fat_sectors as u32),
        (68, END_OF_CHAIN),
    ] {
        put_u32(header, offset, value);
    }
    header[76..].fill(0xff);
    for sector in 0..fat_sectors {
        put_u32(header, 76 + 4 * sector, (first_fat + sector) as u32);
    }
    Ok(file)
}

fn append_chain(file: &mut Vec<u8>, fat: &mut Vec<u32>, bytes: &[u8], size: usize) -> u32 {
    if bytes.is_empty() {
        return END_OF_CHAIN;
    }
    let start = fat.len() as u32;
    let sectors = bytes.len().div_ceil(size) as u32;
    fat.extend(start + 1..start + sectors);
    fat.push(END_OF_CHAIN);
    file.extend_from_slice(bytes);
    file.resize(file.len().next_multiple_of(size), 0);
    start
}

fn sectors_as_bytes(sectors: &[u32]) -> Vec<u8> {
    sectors
        .iter()
        .flat_map(|sector| sector.to_le_bytes())
        .collect()
}

fn write_entry(
    entry: &mut [u8],
    name: &str,
    kind: u8,
    start: u32,
    size: usize,
) -> Result<(), String> {
    let mut length = 2;
    for (index, code) in name.encode_utf16().enumerate() {
        if index >= 31 || code == 0 {
            return Err("invalid compound stream name".into());
        }
        entry[2 * index..2 * index + 2].copy_from_slice(&code.to_le_bytes());
        length += 2;
    }
    entry[64..66].copy_from_slice(&u16::to_le_bytes(length));
    entry[66] = kind;
    entry[67] = 1;
    for offset in [68, 72, 76] {
        put_u32(entry, offset, FREE_SECT);
    }
    put_u32(entry, 116, start);
    entry[120..128].copy_from_slice(&(size as u64).to_le_bytes());
    Ok(())
}

fn write_tree(directory: &mut [u8], start: usize, end: usize, depth: u32, max_depth: u32) -> u32 {
    if start == end {
        return FREE_SECT;
    }
    let middle = (start + end) / 2;
    let left = write_tree(directory, start, middle, depth + 1, max_depth);
    let right = write_tree(directory, middle + 1, end, depth + 1, max_depth);
    let entry = &mut directory[middle * 128..][..128];
    entry[67] = u8::from(depth == 0 || depth < max_depth);
    put_u32(entry, 68, left);
    put_u32(entry, 72, right);
    middle as u32
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
