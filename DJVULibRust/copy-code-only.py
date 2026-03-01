import os
import shutil
from datetime import datetime

# Define extensions to copy
CPP_EXTENSIONS = {".cpp", ".cc", ".cxx", ".c", ".h", ".hpp", ".hh", ".hxx", ".cl", ".cu"}
RUST_EXTENSIONS = {".rs"}

# Specific file names to include (case-insensitive match)
OTHER_FILES = {"Cargo.toml", "Cargo.lock", "CMakeLists.txt"}  # Add more if needed

IGNORE_FOLDERS = {"build", "target"}

def should_copy(filename):
    """Determine if a file should be copied."""
    _, ext = os.path.splitext(filename)

    # Match by extension
    if ext in CPP_EXTENSIONS or ext in RUST_EXTENSIONS:
        return True

    # Case-insensitive match for specific named files
    if filename.lower() in {f.lower() for f in OTHER_FILES}:
        return True

    return False

def copy_code_files(src_root, dst_root):
    """Recursively copy code files excluding ignored folders."""
    print(f"Scanning folder: {src_root}")
    for root, dirs, files in os.walk(src_root):
        print(f" -> Entering directory: {root}")

        # Skip ignored directories
        dirs[:] = [d for d in dirs if d not in IGNORE_FOLDERS]
        print(f"    Keeping subdirectories: {dirs}")

        for file in files:
            full_file_path = os.path.join(root, file)
            rel_path = os.path.relpath(full_file_path, src_root)

            if should_copy(file):
                print(f"    ✅ Should copy: {rel_path}")

                # Replace original extension with .txt
                rel_dir = os.path.dirname(rel_path)
                base_name = os.path.splitext(os.path.basename(rel_path))[0]
                new_filename = f"{base_name}.txt"
                dst_rel_path = os.path.join(rel_dir, new_filename)

                dst_path = os.path.join(dst_root, dst_rel_path)
                os.makedirs(os.path.dirname(dst_path), exist_ok=True)

                # Copy contents to new .txt file
                shutil.copy2(full_file_path, dst_path)
            else:
                print(f"    ❌ Skipping: {rel_path}")

def main():
    parent_dir = os.getcwd()
    print(f"Current working directory: {parent_dir}")
    folder_name = os.path.basename(parent_dir.rstrip(os.sep))

    # Get current date in MM-DD-YY format
    current_date = datetime.now().strftime("%m-%d-%y")

    # Append date to folder name
    output_dir = f"{folder_name}-codeonly-{current_date}"
    full_output_path = os.path.join(parent_dir, output_dir)

    if os.path.exists(full_output_path):
        print(f"Removing existing output directory: {full_output_path}")
        shutil.rmtree(full_output_path)

    print(f"Creating clean code directory at: {full_output_path}")
    copy_code_files(parent_dir, full_output_path)
    print("✅ Code-only directory created successfully.")

if __name__ == "__main__":
    main()