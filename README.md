# Rsync-like File Synchronization Tool

A high-performance file synchronization tool written in Rust that implements the rsync algorithm for efficient file transfers and updates.

## Features

- **Efficient Delta Synchronization**: Uses the rsync algorithm to transfer only the differences between files
- **Rolling Checksum**: Implements Adler-32 variant for fast block matching
- **Strong Hash Verification**: Uses BLAKE3 for cryptographic verification of matching blocks
- **Metadata Preservation**: Maintains file permissions and modification times
- **Recursive Directory Sync**: Automatically handles nested directory structures
- **Memory-Mapped I/O**: Efficient file reading using memory mapping
- **Atomic Updates**: Uses temporary files and atomic renames to prevent corruption

## How It Works

The tool uses a block-based synchronization algorithm similar to rsync:

1. **Block Division**: Files are divided into blocks (8 KiB - 256 KiB, optimized for ~4000 blocks)
2. **Checksum Calculation**: Each block gets a weak checksum (rolling hash) and strong hash (BLAKE3)
3. **Delta Generation**: Source file is scanned using a rolling window to find matching blocks
4. **Efficient Transfer**: Only new/changed data is written; matching blocks are copied from destination
5. **Atomic Update**: Changes are written to a temporary file, then atomically renamed

## Installation

### Prerequisites

- Rust 1.70 or higher
- Cargo

### Build from Source

```bash
git clone https://github.com/yourusername/rsync-tool.git
cd rsync-tool
cargo build --release
```

The compiled binary will be available at `target/release/rsync-tool` (or `rsync-tool.exe` on Windows).

## Usage

```bash
rsync-tool <destination_dir> <source_dir>
```

### Arguments

- `<destination_dir>`: The directory to synchronize to (will be created if it doesn't exist)
- `<source_dir>`: The source directory to synchronize from

### Examples

Synchronize a local directory to a backup location:

```bash
rsync-tool /backup/mydata /home/user/mydata
```

Update a deployment directory:

```bash
rsync-tool /var/www/html ./build
```

## Algorithm Details

### Rolling Checksum

The tool uses a rolling hash (Adler-32 variant) that can be updated in O(1) time as the window slides through the file. This allows efficient detection of matching blocks even when data has been inserted or deleted.

### Block Size Optimization

Block size is automatically calculated based on file size:
- **Minimum**: 8 KiB
- **Maximum**: 256 KiB
- **Target**: ~4000 blocks per file
- **Small files**: Entire file treated as one block

### Delta Operations

Two types of operations are generated:
- **CopyBlock**: Copy an existing block from the destination file
- **InsertData**: Insert new data that doesn't exist in the destination

## Performance Characteristics

- **Best Case**: Files with many unchanged blocks (only metadata updates needed)
- **Worst Case**: Completely new files (equivalent to a full copy)
- **Memory Usage**: Minimal - uses memory mapping and streaming I/O
- **Disk I/O**: Optimized with buffered readers/writers

## Dependencies

- `walkdir`: Recursive directory traversal
- `blake3`: Fast cryptographic hashing
- `memmap2`: Memory-mapped file I/O

## Testing

Run the test suite:

```bash
cargo test
```

The test suite includes:
- Empty file synchronization
- Small and large file handling
- Partial block matching
- Nested directory creation
- Metadata preservation
- Rolling checksum verification
- File change detection

## Limitations

- Does not handle file deletions (destination files are never removed)
- Symbolic links are skipped
- No network transfer support (local filesystem only)
- No compression
- No incremental backup features

## Future Enhancements

- [ ] Support for file deletion synchronization
- [ ] Network protocol implementation
- [ ] Compression support
- [ ] Progress reporting and verbose output
- [ ] Parallel file processing
- [ ] Exclude patterns and filters
- [ ] Dry-run mode

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

This project is licensed under the MIT License - see the LICENSE file for details.

## Acknowledgments

- Inspired by the original rsync algorithm by Andrew Tridgell and Paul Mackerras
- Uses the BLAKE3 cryptographic hash function
- Built with the Rust programming language

## Technical Details

### File Change Detection

Files are considered changed if:
- File sizes differ
- Source modification time is newer than destination

### Atomic Updates

All file updates use a two-phase commit:
1. Write changes to a temporary file (`.tmp.<pid>` suffix)
2. Atomically rename temporary file to destination
3. Clean up temporary file on error

This ensures that destination files are never left in a corrupted state.

### Error Handling

The tool continues processing remaining files even if individual files fail. Errors are logged to stderr with descriptive messages.
