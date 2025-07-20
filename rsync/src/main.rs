use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{UNIX_EPOCH};
use walkdir::WalkDir;


fn get_metadata(file: &Path) -> io::Result<(u64, u64)> {
    let metadata = fs::metadata(file)?;
    let modified_time = metadata.modified()?;
    let duration = modified_time.duration_since(UNIX_EPOCH).unwrap();
    let secs_passed = duration.as_secs();

    Ok((metadata.len(), secs_passed))
}

fn compare_files(src: &Path, dst: &Path) -> io::Result<bool> {
    let (src_len, src_timestamp) = get_metadata(src)?;
    let (dst_len, dst_timestamp) = get_metadata(dst)?;

    if src_timestamp != dst_timestamp {
        return Ok(true);
    }

    if src_len != dst_len {
        return Ok(true);
    }

    Ok(true)
}

fn visit_dirs(src_dir: &str, dst_dir: &str) {
    for entry in WalkDir::new(src_dir) {
        let entry = entry.unwrap();
        let src_path = entry.path();

        let path_as_string = src_path.to_str().unwrap();
        let dest = format!("{}/{}", dst_dir, path_as_string);
        let dest_path = Path::new(dest.as_str());

        if dest_path.exists() {
            let compare_res = match compare_files(src_path, dest_path) {
                Ok(c) => c,
                Err(_e) => continue,
            };

            if compare_res {
                match fs::copy(src_path, dest_path) {
                    Ok(bytes) => println!("Copied {} bytes", bytes),
                    Err(_e) => println!("Failed to copy file!"),
                }
            }
        }
        else {
            match fs::copy(src_path, dest_path) {
                Ok(bytes) => println!("Copied {} bytes", bytes),
                Err(_e) => println!("Failed to copy file!"),
            }
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let dst_dir = &args[1];
    let src_dir = &args[2];

    visit_dirs(src_dir, dst_dir);
}
