// 分析 sparse 文件的 chunk 结构
use std::env;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

const SPARSE_HEADER_SIZE: usize = 28;
const CHUNK_HEADER_SIZE: usize = 12;
const SPARSE_HEADER_MAGIC: u32 = 0xED26FF3A;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <sparse_file>", args[0]);
        return;
    }

    let path = &args[1];
    let mut file = File::open(path).expect("Failed to open file");

    let mut header_buf = [0u8; SPARSE_HEADER_SIZE];
    file.read_exact(&mut header_buf)
        .expect("Failed to read header");

    let magic = u32::from_le_bytes([header_buf[0], header_buf[1], header_buf[2], header_buf[3]]);
    if magic != SPARSE_HEADER_MAGIC {
        eprintln!("Not a sparse file (magic: 0x{:08x})", magic);
        return;
    }

    let blk_sz = u32::from_le_bytes([
        header_buf[12],
        header_buf[13],
        header_buf[14],
        header_buf[15],
    ]);
    let total_blks = u32::from_le_bytes([
        header_buf[16],
        header_buf[17],
        header_buf[18],
        header_buf[19],
    ]);
    let total_chunks = u32::from_le_bytes([
        header_buf[20],
        header_buf[21],
        header_buf[22],
        header_buf[23],
    ]);
    let file_hdr_sz = u16::from_le_bytes([header_buf[8], header_buf[9]]);

    println!("Sparse file: {}", path);
    println!("Block size: {} bytes", blk_sz);
    println!("Total blocks: {}", total_blks);
    println!("Total chunks: {}", total_chunks);
    println!(
        "Expanded size: {} bytes ({:.2} GB)",
        blk_sz as u64 * total_blks as u64,
        (blk_sz as f64 * total_blks as f64) / (1024.0 * 1024.0 * 1024.0)
    );
    println!();

    let mut offset = file_hdr_sz as u64;
    let mut max_chunk_size = 0u64;

    println!("Chunk analysis:");
    println!(
        "{:>5} {:>12} {:>12} {:>12} {:>12}",
        "Index", "Type", "Blocks", "Data Size", "Total Size"
    );
    println!("{}", "-".repeat(60));

    for i in 0..total_chunks {
        file.seek(SeekFrom::Start(offset)).expect("Failed to seek");

        let mut chunk_buf = [0u8; CHUNK_HEADER_SIZE];
        file.read_exact(&mut chunk_buf)
            .expect("Failed to read chunk header");

        let chunk_type = u16::from_le_bytes([chunk_buf[0], chunk_buf[1]]);
        let chunk_sz = u32::from_le_bytes([chunk_buf[4], chunk_buf[5], chunk_buf[6], chunk_buf[7]]);
        let total_sz =
            u32::from_le_bytes([chunk_buf[8], chunk_buf[9], chunk_buf[10], chunk_buf[11]]);

        let type_str = match chunk_type {
            0xCAC1 => "Raw",
            0xCAC2 => "Fill",
            0xCAC3 => "DontCare",
            0xCAC4 => "Crc32",
            _ => "Unknown",
        };

        let data_size = total_sz - CHUNK_HEADER_SIZE as u32;

        println!(
            "{:>5} {:>12} {:>12} {:>12} {:>12}",
            i,
            type_str,
            chunk_sz,
            format_size(data_size as u64),
            format_size(total_sz as u64)
        );

        if total_sz as u64 > max_chunk_size {
            max_chunk_size = total_sz as u64;
        }

        offset += total_sz as u64;
    }

    println!();
    println!(
        "Max chunk size: {} ({:.2} MB)",
        format_size(max_chunk_size),
        max_chunk_size as f64 / (1024.0 * 1024.0)
    );
    println!("max-download-size (768 MB): {} bytes", 768 * 1024 * 1024);

    if max_chunk_size > 768 * 1024 * 1024 {
        println!("WARNING: Some chunks exceed max-download-size!");
    } else {
        println!("OK: All chunks fit within max-download-size");
    }
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    } else {
        format!("{:.2} GB", bytes as f64 / 1024.0 / 1024.0 / 1024.0)
    }
}
