use std::env;
use std::fs;
use std::fs::Metadata;
use std::io;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Read;
use std::io::Write;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::time::SystemTime;
use walkdir::WalkDir;
use std::collections::HashMap;
use memmap2::Mmap;

fn file_changed(src_meta: &fs::Metadata, dst_meta: &fs::Metadata) -> bool {
    let src_len = src_meta.len();
    let dst_len = dst_meta.len();

    if src_len != dst_len { return true; }

    let src_time = src_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let dst_time = dst_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);

    src_time > dst_time
}

fn get_block_size(file_meta: &Metadata) -> usize {
    let file_len = file_meta.len();
    const MIN_BLOCK: usize = 8 * 1024;      // 8 KiB
    const MAX_BLOCK: usize = 256 * 1024;    // 256 KiB
    const TARGET_BLOCKS: u64 = 4000;        // midpoint of 2000–8000


    if file_len < MIN_BLOCK as u64 {
        return file_len as usize;
    }

    // Compute ideal block size based on target number of blocks
    let ideal = (file_len / TARGET_BLOCKS) as usize;

    // Clamp to [MIN_BLOCK, MAX_BLOCK]
    ideal.clamp(MIN_BLOCK, MAX_BLOCK)
}

fn check_hash(dst_strong: &[u8; 32], src_buf: &[u8]) -> bool {
    let src_strong = *(blake3::hash(src_buf).as_bytes());

    *dst_strong == src_strong
}

enum DeltaOp {
    CopyBlock {index: u64},
    InsertData {data: Vec<u8>},
}

fn check_map(block_map: HashMap<u32, Vec<BlockEntry>>, src_path: &Path, block_size: usize) -> io::Result<Vec<DeltaOp>> {
    let file = fs::File::open(src_path)?;
    let mmap = unsafe { Mmap::map(&file)}?;
    if mmap.len() < block_size {
        return Ok(vec![DeltaOp::InsertData { data: mmap.to_vec() }]);
    }
    let mut pos = 0;
    let mut src_block = &mmap[pos..pos + block_size];
    let mut operations: Vec<DeltaOp> = Vec::new();
    let mut rolling_checksum = Rolling::init(src_block);
    let mut byte_acc: Vec<u8> = Vec::new();

'outer: while pos + block_size < mmap.len() {
        let weak = rolling_checksum.checksum();

        let block_matched = block_map.get(&weak);

        if block_matched.is_some() {
            let dst_blocks = block_matched.unwrap();

            for block in dst_blocks {
                if check_hash(&block.strong, src_block) {

                    if !byte_acc.is_empty() {
                        operations.push(DeltaOp::InsertData { data: std::mem::take(&mut byte_acc) });
                        byte_acc.clear();
                    }
                    operations.push(DeltaOp::CopyBlock { index: block.index as u64});

                    pos += block_size;
                    if pos + block_size > mmap.len() {
                        byte_acc.extend_from_slice(&mmap[pos..mmap.len()]);
                        break 'outer;
                    }

                    src_block = &mmap[pos..pos + block_size];
                    rolling_checksum = Rolling::init(src_block);
                    continue 'outer;
                }
            }
        }

        byte_acc.push(mmap[pos]);
        rolling_checksum.roll(mmap[pos], mmap[pos + block_size]);
        pos += 1;
    }

    if !byte_acc.is_empty() {
        operations.push(DeltaOp::InsertData { data: byte_acc });
    }

    Ok(operations)
}

fn process_ops(ops: Vec<DeltaOp>, dst_path: &Path, block_size: usize, temp_path: &Path) -> io::Result<()> {
    let dst_file = fs::File::open(dst_path)?;
    let out_file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(temp_path)?;

    let mut reader = BufReader::new(dst_file);
    let mut writer = BufWriter::new(out_file);

    let mut buf = vec![0u8; block_size];

    for op in ops {
        match op {
            DeltaOp::CopyBlock { index } => {
                let offset = index * block_size as u64;
                reader.seek(SeekFrom::Start(offset))?;

                let n = reader.read(&mut buf)?;
                writer.write_all(&buf[..n])?;
            }
            DeltaOp::InsertData { data } => {
                writer.write_all(&data)?;
            }
        }
    }

    writer.flush()?; // flush BufWriter
    writer.get_ref().sync_all()?;
    fs::rename(temp_path, dst_path)?;

    Ok(())
}

fn visit_dirs(src_dir: &Path, dst_dir: &Path) {
    for entry in WalkDir::new(src_dir).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_symlink() { continue; }

        let src_path = entry.path();

        let rel = match src_path.strip_prefix(src_dir) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("strip_prefix failed for {}: {}", src_path.display(), e);
                continue;
            }
        };

        let dst_path = Path::new(dst_dir).join(rel);

        if entry.file_type().is_dir() {
            if !dst_path.exists() {
                match fs::create_dir_all(&dst_path) {
                    Ok(_) => println!("Created directory"),
                    Err(e) => println!("Failed to copy file becasue of {}!", e),
                }
            }
            continue;
        }

        if let Some(parent) = dst_path.parent() {
            if !parent.exists() {
                if let Err(e) = fs::create_dir_all(parent) {
                    eprintln!("Failed to create parent dir '{}': {}", parent.display(), e);
                    continue;
                }
            }
        } else {
            eprintln!("No parent for destination path '{}'", dst_path.display());
            continue;
        }

        if dst_path.exists() {
            let src_meta = match entry.metadata() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Failed to get metadata because of {}", e);
                    continue;
                }
            };
            let dst_meta = match fs::metadata(&dst_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Failed to get metadata because of {}", e);
                    continue;
                }
            };

            let compare_res = file_changed(&src_meta, &dst_meta);

            if compare_res {
                let block_size = get_block_size(&dst_meta);
                let block_map = match build_block_map(&dst_path, block_size) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("Failed to get block_map because of {}", e);
                        continue;
                    }
                };

                let ops = match check_map(block_map, src_path, block_size) {
                    Ok(x) => x,
                    Err(e) => {
                        eprint!("Failed to check block_map because of {}", e);
                        continue;
                    }
                };

                let temp_path = dst_path.with_extension("tmp");
                let _res = match process_ops(ops, &dst_path, block_size, &temp_path) {
                    Ok(x) => x,
                    Err(e) => {
                        eprint!("Failed to update dst file because of {}", e);
                        continue;
                    }
                };
            }
        }
        else {
            match fs::copy(src_path, dst_path) {
                Ok(bytes) => println!("Copied {} bytes", bytes),
                Err(e) => println!("Failed to copy file becasue of {}!", e),
            }
        }
    }
}

struct Rolling {
    a: u32,
    b: u32,
    n: usize
}

impl Rolling {
    const M: u32 = 65536;

    fn init(window: &[u8]) -> Self {
        let mut a: u32 = 0;
        let mut b: u32 = 0;

        for &byte in window {
            a = (a + byte as u32) % Self::M;
            b = (b + a) % Self::M;
        }

        Self {a, b, n: window.len()}
    }

    fn checksum(&self) -> u32 {
        (self.b << 16) | self.a
    }

    fn roll(&mut self, out: u8, inp: u8) {
        // a' = (a - out + inp) mod M
        let a = (self.a + Self::M + inp as u32 - out as u32) % Self::M;

        // b' = (b - n*out + a') mod M
        let n_out = ((self.n as u32) * (out as u32)) % Self::M;
        let b = (self.b + Self::M - n_out + a) % Self::M;

        self.a = a;
        self.b = b;
    }

}

struct BlockEntry {
    index: u64,
    strong: [u8; 32],
}

fn build_block_map(dest_file: &Path, block_size: usize) -> io::Result<HashMap<u32, Vec<BlockEntry>>> {
    let mut res: HashMap<u32, Vec<BlockEntry>> = HashMap::new();
    let mut file = fs::File::open(dest_file)?;
    let mut buf = vec![0u8; block_size];
    let mut i: u64 = 0;

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 { break; }

        let rolling_checksum = Rolling::init(&buf[..n]);

        let weak = rolling_checksum.checksum();
        let strong = blake3::hash(&buf[..n]);

        let block: BlockEntry = BlockEntry { index: i, strong: *strong.as_bytes() };

        res.entry(weak).or_default().push(block);

        if n < block_size { break; }

        i += 1;
    }

    Ok(res)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: tool <dst_dir> <src_dir>");
        std::process::exit(1);
    }

    let dst_dir = Path::new(&args[1]);
    let src_dir = Path::new(&args[2]);


    if !src_dir.exists() {
        eprintln!("Source directory does not exist: {}", src_dir.display());
        std::process::exit(1);
    }

    if !dst_dir.exists() {
        if let Err(e) = fs::create_dir_all(dst_dir) {
            eprintln!("Failed to create destination directory '{}': {}", dst_dir.display(), e);
            std::process::exit(1);
        }
    }

    visit_dirs(src_dir, dst_dir);
}
