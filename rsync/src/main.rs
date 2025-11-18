use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{UNIX_EPOCH};
use walkdir::WalkDir;
use std::collections::HashMap;
// use sha2::{Sha256, Digest};
use adler::Adler32;


fn get_metadata(file: &Path) -> io::Result<(u64, u64)> {
    let metadata = fs::metadata(file)?;
    let modified_time = metadata.modified()?;
    let duration = modified_time.duration_since(UNIX_EPOCH).unwrap();
    let secs_passed = duration.as_secs();

    Ok((metadata.len(), secs_passed))
}

fn file_changed(src: &Path, dst: &Path) -> io::Result<bool> {
    let (src_len, src_timestamp) = get_metadata(src)?;
    let (dst_len, dst_timestamp) = get_metadata(dst)?;

    if src_timestamp != dst_timestamp {
        return Ok(true);
    }

    if src_len != dst_len {
        return Ok(true);
    }

    Ok(false)
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

            let compare_res = match file_changed(src_path, &dst_path) {
                Ok(c) => c,
                Err(_e) => continue,
            };

            if compare_res {
                match fs::copy(src_path, dst_path) {
                    Ok(bytes) => println!("Copied {} bytes", bytes),
                    Err(_e) => println!("Failed to copy file!"),
                }
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

// fn build_block_map(dest_file: &Path) {



// }

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
