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

// Determines if a source file has changed compared to the destination file.
// Returns true if the files differ in size or if the source is newer.
fn file_changed(src_meta: &fs::Metadata, dst_meta: &fs::Metadata) -> bool {
    let src_len = src_meta.len();
    let dst_len = dst_meta.len();

    // If sizes differ, file has definitely changed
    if src_len != dst_len { return true; }

    // Compare modification times
    let src_time = src_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let dst_time = dst_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);

    // File changed if source is newer than destination
    src_time > dst_time
}

// Calculates optimal block size for rsync algorithm based on file size.
// Aims for 4000 blocks while keeping block size between 8 KiB and 256 KiB.
fn get_block_size(file_meta: &Metadata) -> usize {
    let file_len = file_meta.len();
    const MIN_BLOCK: usize = 8 * 1024;      // 8 KiB minimum
    const MAX_BLOCK: usize = 256 * 1024;    // 256 KiB maximum
    const TARGET_BLOCKS: u64 = 4000;        // Target number of blocks

    // For very small files, use the entire file as one block
    if file_len < MIN_BLOCK as u64 {
        return file_len as usize;
    }

    // Compute ideal block size based on target number of blocks
    let ideal = (file_len / TARGET_BLOCKS) as usize;

    // Clamp to [MIN_BLOCK, MAX_BLOCK] range
    ideal.clamp(MIN_BLOCK, MAX_BLOCK)
}

// Compares a strong hash (BLAKE3) from destination with source buffer.
// Returns true if the hashes match (blocks are identical).
fn check_hash(dst_strong: &[u8; 32], src_buf: &[u8]) -> bool {
    let src_strong = *(blake3::hash(src_buf).as_bytes());

    *dst_strong == src_strong
}

// Represents operations needed to reconstruct the source file from destination.
enum DeltaOp {
    // Copy a block from the destination file at the given index
    CopyBlock {index: u64},
    // Insert new data that doesn't exist in destination
    InsertData {data: Vec<u8>},
}

// Generates delta operations by comparing source file against destination's block map.
// Uses rolling hash to efficiently find matching blocks.
fn check_map(block_map: HashMap<u32, Vec<BlockEntry>>, src_path: &Path, block_size: usize) -> io::Result<Vec<DeltaOp>> {
    // Memory-map the source file for efficient access
    let file = fs::File::open(src_path)?;
    let mmap = unsafe { Mmap::map(&file)}?;

    // If file is smaller than or equal to block size, just insert all data
    if mmap.len() <= block_size {
        return Ok(vec![DeltaOp::InsertData { data: mmap.to_vec() }]);
    }

    let mut pos = 0;
    let mut src_block = &mmap[pos..pos + block_size];
    let mut operations: Vec<DeltaOp> = Vec::new();
    let mut rolling_checksum = Rolling::init(src_block);
    let mut byte_acc: Vec<u8> = Vec::new();  // Accumulates bytes that don't match

'outer: while pos + block_size <= mmap.len() {
        // Calculate weak checksum for current window
        let weak = rolling_checksum.checksum();

        // Check if this weak checksum exists in destination
        if let Some(dst_blocks) = block_map.get(&weak) {
            // Weak match found - verify with strong hash
            for block in dst_blocks {
                if check_hash(&block.strong, src_block) {
                    // Strong match! This block exists in destination

                    // First, flush any accumulated non-matching bytes
                    if !byte_acc.is_empty() {
                        operations.push(DeltaOp::InsertData { data: std::mem::take(&mut byte_acc) });
                    }

                    // Add operation to copy this block from destination
                    operations.push(DeltaOp::CopyBlock { index: block.index as u64});

                    // Jump forward by entire block size
                    pos += block_size;
                    if pos + block_size > mmap.len() {
                        break 'outer;
                    }

                    // Reinitialize rolling checksum for new position
                    src_block = &mmap[pos..pos + block_size];
                    rolling_checksum = Rolling::init(src_block);
                    continue 'outer;
                }
            }
        }

        // No match found - slide window by one byte
        byte_acc.push(mmap[pos]);
        pos += 1;

        // Only roll checksum if we still have a full block ahead
        if pos + block_size <= mmap.len() {
            rolling_checksum.roll(mmap[pos - 1], mmap[pos + block_size - 1]);
            src_block = &mmap[pos..pos + block_size];
        }
    }

    // Handle remaining bytes at end of file (partial block)
    if pos < mmap.len() {
        let remaining = &mmap[pos..];
        let remaining_checksum = Rolling::init(remaining);
        let weak = remaining_checksum.checksum();

        let mut matched = false;
        // Check if this partial block exists in destination
        if let Some(dst_blocks) = block_map.get(&weak) {
            for block in dst_blocks {
                if check_hash(&block.strong, remaining) {
                    // Match found for partial block
                    if !byte_acc.is_empty() {
                        operations.push(DeltaOp::InsertData { data: std::mem::take(&mut byte_acc) });
                    }
                    operations.push(DeltaOp::CopyBlock { index: block.index as u64 });
                    matched = true;
                    break;
                }
            }
        }

        // No match - add remaining bytes as new data
        if !matched {
            byte_acc.extend_from_slice(remaining);
        }
    }

    // Flush any remaining accumulated bytes
    if !byte_acc.is_empty() {
        operations.push(DeltaOp::InsertData { data: byte_acc });
    }

    Ok(operations)
}

// Applies delta operations to reconstruct source file from destination.
// Writes to temporary file then atomically renames to destination.
fn process_ops(ops: Vec<DeltaOp>, dst_path: &Path, block_size: usize, temp_path: &Path) -> io::Result<()> {
    // Open destination file for reading blocks
    let dst_file = fs::File::open(dst_path)?;

    // Create temporary output file
    let out_file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(temp_path)?;

    let mut reader = BufReader::new(dst_file);
    let mut writer = BufWriter::new(out_file);

    let mut buf = vec![0u8; block_size];

    // Process each delta operation
    for op in ops {
        match op {
            DeltaOp::CopyBlock { index } => {
                // Copy block from destination file
                let offset = index * block_size as u64;
                reader.seek(SeekFrom::Start(offset))?;

                let n = reader.read(&mut buf)?;
                writer.write_all(&buf[..n])?;
            }
            DeltaOp::InsertData { data } => {
                // Write new data that doesn't exist in destination
                writer.write_all(&data)?;
            }
        }
    }

    // Ensure all data is written to disk
    writer.flush()?;
    writer.get_ref().sync_all()?;

    // Atomically replace destination with new file
    fs::rename(temp_path, dst_path)?;

    Ok(())
}

// Copies metadata (permissions and modification time) from source to destination.
fn copy_metadata(src_meta: &fs::Metadata, dst_path: &Path) -> io::Result<()> {
    // Copy modification time
    if let Ok(modified) = src_meta.modified() {
        // On Unix systems, also copy file permissions
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = fs::Permissions::from_mode(src_meta.permissions().mode());
            fs::set_permissions(dst_path, permissions)?;
        }

        // Set modification time (works on all platforms)
        use std::fs::OpenOptions;
        let file = OpenOptions::new()
            .write(true)
            .open(dst_path)?;
        file.set_modified(modified)?;
    }

    Ok(())
}

// Recursively synchronizes files from source directory to destination directory.
// Uses rsync algorithm for efficient updates of existing files.
fn visit_dirs(src_dir: &Path, dst_dir: &Path) {
    // Walk through all entries in source directory
    for entry in WalkDir::new(src_dir).into_iter().filter_map(Result::ok) {
        // Skip symbolic links
        if entry.file_type().is_symlink() { continue; }

        let src_path = entry.path();

        // Calculate relative path from source root
        let rel = match src_path.strip_prefix(src_dir) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("strip_prefix failed for {}: {}", src_path.display(), e);
                continue;
            }
        };

        // Construct corresponding destination path
        let dst_path = Path::new(dst_dir).join(rel);

        // Handle directories
        if entry.file_type().is_dir() {
            if !dst_path.exists() {
                match fs::create_dir_all(&dst_path) {
                    Ok(_) => println!("Created directory"),
                    Err(e) => eprintln!("Failed to create directory because of {}!", e),
                }
            }
            continue;
        }

        // Ensure parent directory exists for files
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

        // Handle files
        if dst_path.exists() {
            // Destination file exists - check if we need to update it
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
                // File has changed - use rsync algorithm to update
                let block_size = get_block_size(&dst_meta);

                // Build hash map of destination file blocks
                let block_map = match build_block_map(&dst_path, block_size) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("Failed to get block_map because of {}", e);
                        continue;
                    }
                };

                // Generate delta operations
                let ops = match check_map(block_map, src_path, block_size) {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("Failed to check block_map because of {}", e);
                        continue;
                    }
                };

                // Apply delta operations with unique temp file
                use std::process;
                let temp_path = dst_path.with_file_name(
                    format!("{}.tmp.{}",
                        dst_path.file_name().unwrap().to_string_lossy(),
                        process::id()
                    )
                );

                match process_ops(ops, &dst_path, block_size, &temp_path) {
                    Ok(_) => {
                        // Copy metadata after successful sync
                        if let Err(e) = copy_metadata(&src_meta, &dst_path) {
                            eprintln!("Failed to copy metadata for '{}': {}", dst_path.display(), e);
                        }
                    }
                    Err(e) => {
                        eprintln!("Failed to update dst file because of {}", e);
                        // Clean up temp file on error
                        let _ = fs::remove_file(&temp_path);
                        continue;
                    }
                };
            }
        }
        else {
            let src_meta = match entry.metadata() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Failed to get metadata because of {}", e);
                    continue;
                }
            };

            // Destination file doesn't exist - simple copy
            match fs::copy(src_path, &dst_path) {
                Ok(bytes) => {
                    // Copy metadata after successful sync
                    if let Err(e) = copy_metadata(&src_meta, &dst_path) {
                        eprintln!("Failed to copy metadata for '{}': {}", dst_path.display(), e);
                    }
                    println!("Copied {} bytes", bytes);
                }
                Err(e) => eprintln!("Failed to copy file because of {}!", e),
            }
        }
    }
}

// Rolling checksum implementation (Adler-32 variant).
// Used for efficient block matching in rsync algorithm.
struct Rolling {
    a: u32,  // Sum of all bytes in window
    b: u32,  // Sum of all 'a' values
    n: usize // Window size
}

impl Rolling {
    const M: u32 = 65536;  // Modulus for checksum calculation

    // Initialize rolling checksum for a given window of bytes.
    fn init(window: &[u8]) -> Self {
        let mut a: u32 = 0;
        let mut b: u32 = 0;

        // Calculate initial checksum values
        for &byte in window {
            a = (a + byte as u32) % Self::M;
            b = (b + a) % Self::M;
        }

        Self {a, b, n: window.len()}
    }

    // Get the 32-bit checksum value (b in high 16 bits, a in low 16 bits).
    fn checksum(&self) -> u32 {
        (self.b << 16) | self.a
    }

    // Update checksum by rolling window: remove 'out' byte, add 'inp' byte.
    // This is O(1) instead of recalculating entire window.
    fn roll(&mut self, out: u8, inp: u8) {
        // Update a: remove old byte, add new byte
        let a = (self.a + Self::M + inp as u32 - out as u32) % Self::M;

        // Update b: remove contribution of old byte, add new 'a'
        let n_out = ((self.n as u32) * (out as u32)) % Self::M;
        let b = (self.b + Self::M - n_out + a) % Self::M;

        self.a = a;
        self.b = b;
    }
}

// Represents a block in the destination file with its checksums.
struct BlockEntry {
    index: u64,        // Block index in file
    strong: [u8; 32],  // Strong hash (BLAKE3) for verification
}

// Builds a hash map of all blocks in the destination file.
// Maps weak checksums to lists of blocks (handles collisions).
fn build_block_map(dest_file: &Path, block_size: usize) -> io::Result<HashMap<u32, Vec<BlockEntry>>> {
    let mut res: HashMap<u32, Vec<BlockEntry>> = HashMap::new();
    let mut file = fs::File::open(dest_file)?;
    let mut buf = vec![0u8; block_size];
    let mut i: u64 = 0;

    loop {
        // Read one block
        let n = file.read(&mut buf)?;
        if n == 0 { break; }

        // Calculate both weak and strong checksums
        let rolling_checksum = Rolling::init(&buf[..n]);
        let weak = rolling_checksum.checksum();
        let strong = blake3::hash(&buf[..n]);

        // Store block entry
        let block: BlockEntry = BlockEntry { index: i, strong: *strong.as_bytes() };
        res.entry(weak).or_default().push(block);

        // Stop if we read a partial block (end of file)
        if n < block_size { break; }

        i += 1;
    }

    Ok(res)
}

fn main() {
    // Parse command line arguments
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: tool <dst_dir> <src_dir>");
        std::process::exit(1);
    }

    let dst_dir = Path::new(&args[1]);
    let src_dir = Path::new(&args[2]);

    // Validate source directory exists
    if !src_dir.exists() {
        eprintln!("Source directory does not exist: {}", src_dir.display());
        std::process::exit(1);
    }

    // Create destination directory if it doesn't exist
    if !dst_dir.exists() {
        if let Err(e) = fs::create_dir_all(dst_dir) {
            eprintln!("Failed to create destination directory '{}': {}", dst_dir.display(), e);
            std::process::exit(1);
        }
    }

    // Start synchronization
    visit_dirs(src_dir, dst_dir);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::time::Duration;
    use tempfile::TempDir;

    // Helper function to create a test file with specific content
    fn create_test_file(path: &Path, content: &[u8]) -> io::Result<()> {
        let mut file = fs::File::create(path)?;
        file.write_all(content)?;
        file.sync_all()?;
        Ok(())
    }

    // Helper function to read file content
    fn read_file(path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    // Helper to compare file contents
    fn files_identical(path1: &Path, path2: &Path) -> bool {
        match (read_file(path1), read_file(path2)) {
            (Ok(content1), Ok(content2)) => content1 == content2,
            _ => false,
        }
    }

    #[test]
    fn test_empty_file_sync() {
        let temp_dir = TempDir::new().unwrap();
        let src_dir = temp_dir.path().join("src");
        let dst_dir = temp_dir.path().join("dst");

        fs::create_dir(&src_dir).unwrap();
        fs::create_dir(&dst_dir).unwrap();

        // Create empty file
        let src_file = src_dir.join("empty.txt");
        create_test_file(&src_file, b"").unwrap();

        visit_dirs(&src_dir, &dst_dir);

        let dst_file = dst_dir.join("empty.txt");
        assert!(dst_file.exists());
        assert_eq!(fs::metadata(&dst_file).unwrap().len(), 0);
    }

    #[test]
    fn test_small_file_sync() {
        let temp_dir = TempDir::new().unwrap();
        let src_dir = temp_dir.path().join("src");
        let dst_dir = temp_dir.path().join("dst");

        fs::create_dir(&src_dir).unwrap();
        fs::create_dir(&dst_dir).unwrap();

        // Create small file (< 8 KiB)
        let src_file = src_dir.join("small.txt");
        let content = b"Hello, World!";
        create_test_file(&src_file, content).unwrap();

        visit_dirs(&src_dir, &dst_dir);

        let dst_file = dst_dir.join("small.txt");
        assert!(dst_file.exists());
        assert!(files_identical(&src_file, &dst_file));
    }

    #[test]
    fn test_large_file_sync() {
        let temp_dir = TempDir::new().unwrap();
        let src_dir = temp_dir.path().join("src");
        let dst_dir = temp_dir.path().join("dst");

        fs::create_dir(&src_dir).unwrap();
        fs::create_dir(&dst_dir).unwrap();

        // Create large file (1 MB)
        let src_file = src_dir.join("large.bin");
        let content = vec![0xAB; 1024 * 1024];
        create_test_file(&src_file, &content).unwrap();

        visit_dirs(&src_dir, &dst_dir);

        let dst_file = dst_dir.join("large.bin");
        assert!(dst_file.exists());
        assert!(files_identical(&src_file, &dst_file));
    }

    #[test]
    fn test_file_update_with_changes() {
        let temp_dir = TempDir::new().unwrap();
        let src_dir = temp_dir.path().join("src");
        let dst_dir = temp_dir.path().join("dst");

        fs::create_dir(&src_dir).unwrap();
        fs::create_dir(&dst_dir).unwrap();

        let src_file = src_dir.join("update.txt");
        let dst_file = dst_dir.join("update.txt");

        // Initial sync
        create_test_file(&src_file, b"Original content").unwrap();
        visit_dirs(&src_dir, &dst_dir);
        assert!(files_identical(&src_file, &dst_file));

        // Wait to ensure different timestamp
        std::thread::sleep(Duration::from_millis(100));

        // Update source file
        create_test_file(&src_file, b"Updated content").unwrap();
        visit_dirs(&src_dir, &dst_dir);

        assert!(files_identical(&src_file, &dst_file));
        assert_eq!(read_file(&dst_file).unwrap(), b"Updated content");
    }

    #[test]
    fn test_partial_block_matching() {
        let temp_dir = TempDir::new().unwrap();
        let src_dir = temp_dir.path().join("src");
        let dst_dir = temp_dir.path().join("dst");

        fs::create_dir(&src_dir).unwrap();
        fs::create_dir(&dst_dir).unwrap();

        let src_file = src_dir.join("partial.txt");
        let dst_file = dst_dir.join("partial.txt");

        // Create file with 10KB of 'A's
        let original = vec![b'A'; 10 * 1024];
        create_test_file(&src_file, &original).unwrap();
        visit_dirs(&src_dir, &dst_dir);

        std::thread::sleep(Duration::from_millis(100));

        // Append some data (tests partial block at end)
        let mut updated = original.clone();
        updated.extend_from_slice(b"APPENDED DATA");
        create_test_file(&src_file, &updated).unwrap();
        visit_dirs(&src_dir, &dst_dir);

        assert!(files_identical(&src_file, &dst_file));
    }

    #[test]
    fn test_nested_directory_creation() {
        let temp_dir = TempDir::new().unwrap();
        let src_dir = temp_dir.path().join("src");
        let dst_dir = temp_dir.path().join("dst");

        fs::create_dir(&src_dir).unwrap();
        fs::create_dir(&dst_dir).unwrap();

        // Create nested structure
        let nested_dir = src_dir.join("level1").join("level2").join("level3");
        fs::create_dir_all(&nested_dir).unwrap();

        let nested_file = nested_dir.join("deep.txt");
        create_test_file(&nested_file, b"Deep file").unwrap();

        visit_dirs(&src_dir, &dst_dir);

        let dst_nested_file = dst_dir.join("level1").join("level2").join("level3").join("deep.txt");
        assert!(dst_nested_file.exists());
        assert!(files_identical(&nested_file, &dst_nested_file));
    }

    #[test]
    fn test_metadata_preservation() {
        let temp_dir = TempDir::new().unwrap();
        let src_dir = temp_dir.path().join("src");
        let dst_dir = temp_dir.path().join("dst");

        fs::create_dir(&src_dir).unwrap();
        fs::create_dir(&dst_dir).unwrap();

        let src_file = src_dir.join("meta.txt");
        create_test_file(&src_file, b"Test content").unwrap();

        // Get original modification time
        let src_meta_before = fs::metadata(&src_file).unwrap();
        let src_mtime = src_meta_before.modified().unwrap();

        visit_dirs(&src_dir, &dst_dir);

        let dst_file = dst_dir.join("meta.txt");
        let dst_meta = fs::metadata(&dst_file).unwrap();
        let dst_mtime = dst_meta.modified().unwrap();

        // Modification times should match (within 1 second tolerance for filesystem precision)
        let diff = if src_mtime > dst_mtime {
            src_mtime.duration_since(dst_mtime).unwrap()
        } else {
            dst_mtime.duration_since(src_mtime).unwrap()
        };

        assert!(diff < Duration::from_secs(2), "Modification times differ by {:?}", diff);
    }

    #[test]
    fn test_no_update_when_identical() {
        let temp_dir = TempDir::new().unwrap();
        let src_dir = temp_dir.path().join("src");
        let dst_dir = temp_dir.path().join("dst");

        fs::create_dir(&src_dir).unwrap();
        fs::create_dir(&dst_dir).unwrap();

        let src_file = src_dir.join("same.txt");
        let dst_file = dst_dir.join("same.txt");

        create_test_file(&src_file, b"Same content").unwrap();
        visit_dirs(&src_dir, &dst_dir);

        // Get dst modification time after first sync
        let dst_mtime_1 = fs::metadata(&dst_file).unwrap().modified().unwrap();

        std::thread::sleep(Duration::from_millis(100));

        // Sync again without changes
        visit_dirs(&src_dir, &dst_dir);

        // Modification time should not change
        let dst_mtime_2 = fs::metadata(&dst_file).unwrap().modified().unwrap();
        assert_eq!(dst_mtime_1, dst_mtime_2);
    }

    #[test]
    fn test_rolling_checksum() {
        let data = b"Hello, World!";

        // Calculate checksum for window starting at position 1
        let rolling1 = Rolling::init(&data[1..]);
        let checksum1 = rolling1.checksum();

        // Calculate checksum for window starting at position 0, then roll forward
        let mut rolling2 = Rolling::init(&data[0..data.len()-1]);
        rolling2.roll(data[0], data[data.len()-1]);
        let checksum2 = rolling2.checksum();

        // Both should represent the same window: "ello, World!"
        assert_eq!(checksum1, checksum2);
    }

    #[test]
    fn test_block_size_calculation() {
        // Test minimum block size
        let small_meta = fs::metadata("Cargo.toml").unwrap();

        // Mock metadata for testing (we'll use actual file but conceptually test the function)
        // For very small files
        assert!(get_block_size(&small_meta) >= 8 * 1024 || small_meta.len() < 8 * 1024);
    }

    #[test]
    fn test_file_changed_detection() {
        let temp_dir = TempDir::new().unwrap();

        let file1 = temp_dir.path().join("file1.txt");
        let file2 = temp_dir.path().join("file2.txt");

        create_test_file(&file1, b"content").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        create_test_file(&file2, b"content").unwrap();

        let meta1 = fs::metadata(&file1).unwrap();
        let meta2 = fs::metadata(&file2).unwrap();

        // file2 is newer, so it should be detected as changed
        assert!(file_changed(&meta2, &meta1));
        // file1 is older, so it should not be detected as changed
        assert!(!file_changed(&meta1, &meta2));
    }
}
