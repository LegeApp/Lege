#!/bin/bash
# PDF validation script for JBIG2 encoder testing
# Usage: ./validate_pdfs.sh <output_directory>

if [ $# -eq 0 ]; then
    echo "Usage: $0 <output_directory>"
    echo "Example: $0 test_output_pdfs/run_123456789"
    exit 1
fi

OUTPUT_DIR="$1"

if [ ! -d "$OUTPUT_DIR" ]; then
    echo "Error: Directory $OUTPUT_DIR does not exist"
    exit 1
fi

echo "=========================================="
echo "JBIG2 PDF Validation Report"
echo "=========================================="
echo "Directory: $OUTPUT_DIR"
echo "Timestamp: $(date)"
echo ""

# Check for required tools
echo "Tool Availability:"
command -v jbig2dec >/dev/null 2>&1 && echo "✅ jbig2dec: Available" || echo "❌ jbig2dec: Not found"
command -v pdfimages >/dev/null 2>&1 && echo "✅ pdfimages: Available" || echo "❌ pdfimages: Not found"
command -v file >/dev/null 2>&1 && echo "✅ file: Available" || echo "❌ file: Not found"
echo ""

# Validate each PDF
for pdf in "$OUTPUT_DIR"/*.pdf; do
    if [ ! -f "$pdf" ]; then
        echo "No PDF files found in $OUTPUT_DIR"
        exit 0
    fi
    
    pdf_name=$(basename "$pdf")
    echo "------------------------------------------"
    echo "Validating: $pdf_name"
    echo "------------------------------------------"
    
    # Check file type
    file_type=$(file "$pdf")
    echo "File type: $file_type"
    
    # Check file size
    file_size=$(stat -c%s "$pdf")
    echo "File size: $file_size bytes"
    
    # Extract JBIG2 streams if pdfimages is available
    if command -v pdfimages >/dev/null 2>&1; then
        echo "Extracting JBIG2 streams..."
        extract_dir="$OUTPUT_DIR/extracted_${pdf_name%.pdf}"
        mkdir -p "$extract_dir"
        
        if pdfimages -jbig2 "$pdf" "$extract_dir/extracted" 2>/dev/null; then
            echo "✅ Extraction successful"
            
            # List extracted files
            echo "Extracted files:"
            ls -la "$extract_dir"/extracted* 2>/dev/null || echo "No files extracted"
            
            # Try to decode with jbig2dec if available
            if command -v jbig2dec >/dev/null 2>&1; then
                echo "Testing with jbig2dec..."
                
                global_file=$(ls "$extract_dir"/extracted*.jb2g 2>/dev/null | head -1)
                page_file=$(ls "$extract_dir"/extracted*.jb2e 2>/dev/null | head -1)
                
                if [ -n "$global_file" ] && [ -n "$page_file" ]; then
                    output_file="$extract_dir/decoded.pbm"
                    if jbig2dec "$global_file" "$page_file" -o "$output_file" 2>"$extract_dir/jbig2dec_errors.txt"; then
                        echo "✅ jbig2dec decoding successful"
                        echo "Output file: $(stat -c%s "$output_file" 2>/dev/null || echo "N/A") bytes"
                    else
                        echo "❌ jbig2dec decoding failed"
                        echo "Errors:"
                        cat "$extract_dir/jbig2dec_errors.txt" | head -5
                    fi
                elif [ -n "$page_file" ]; then
                    # Single file (lossless mode)
                    output_file="$extract_dir/decoded.pbm"
                    if jbig2dec "$page_file" -o "$output_file" 2>"$extract_dir/jbig2dec_errors.txt"; then
                        echo "✅ jbig2dec decoding successful (single file)"
                        echo "Output file: $(stat -c%s "$output_file" 2>/dev/null || echo "N/A") bytes"
                    else
                        echo "❌ jbig2dec decoding failed (single file)"
                        echo "Errors:"
                        cat "$extract_dir/jbig2dec_errors.txt" | head -5
                    fi
                else
                    echo "❌ No JBIG2 streams found"
                fi
            else
                echo "⚠️  jbig2dec not available for decoding test"
            fi
        else
            echo "❌ Extraction failed"
        fi
    else
        echo "⚠️  pdfimages not available - skipping stream extraction"
    fi
    
    echo ""
done

echo "=========================================="
echo "Validation Summary"
echo "=========================================="
echo "Total PDFs processed: $(ls -1 "$OUTPUT_DIR"/*.pdf 2>/dev/null | wc -l)"
echo "Check individual results above"
echo ""
echo "Common Issues:"
echo "- jbig2dec 'out of range symbol ID' errors: Often version compatibility issues"
echo "- 'OOB obtained' errors: May indicate encoder/decoder format differences"
echo "- 'not a JBIG2 file header': Expected for lossless mode (single stream)"
echo ""
echo "Recommendations:"
echo "1. Test PDFs in multiple viewers (Adobe, Chrome, Edge)"
echo "2. Compare with reference C encoder output if available"
echo "3. Check jbig2dec version compatibility"
