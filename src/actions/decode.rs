use crate::cli::error::DecodingError;
use crate::cli::opts::Decode;
use crate::lib::deter;
use pdf::file::File as PDFFile;
use pdf::object::{Resolve, XObject};
use std::env;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;
use zip::ZipArchive;

/// Perform a decoding using the provided configuration object
pub fn decode(dec: &Decode) -> Result<Vec<PathBuf>, DecodingError> {
    // Get absolute path to the input for path manipulation
    let input = env::current_dir()
        .map_err(DecodingError::FailedToGetCWD)?
        .join(&dec.input);

    // Check if the input file exists
    if !input.exists() {
        return Err(DecodingError::InputFileNotFound);
    } else if !input.is_file() {
        return Err(DecodingError::InputFileIsADirectory);
    }

    // Create the output directory if needed, and get the output path
    let output = match &dec.output {
        Some(output) => {
            if !output.exists() {
                if dec.create_output_dir {
                    fs::create_dir_all(output)
                        .map_err(DecodingError::FailedToCreateOutputDirectory)?
                } else {
                    return Err(DecodingError::OutputDirectoryNotFound);
                }
            } else if !output.is_dir() {
                return Err(DecodingError::OutputDirectoryIsAFile);
            }

            output.to_owned()
        }

        None => {
            let path = input.with_extension("");
            fs::create_dir_all(&path).map_err(DecodingError::FailedToCreateOutputDirectory)?;
            path
        }
    };

    // Get the input file's extension to determine its format
    let ext = input
        .extension()
        .ok_or_else(|| DecodingError::UnsupportedFormat(String::new()))?;

    let ext = ext
        .to_str()
        .ok_or_else(|| DecodingError::InputFileHasInvalidUTF8FileExtension(
            input.file_name().unwrap().to_os_string(),
        ))?;

    // Get timestamp to measure decoding time
    let extraction_started = Instant::now();

    // Decode
    let result = match ext.to_lowercase().as_str() {
        "zip" | "cbz" => {
            debug!("Matched input format: ZIP / CBZ");
            trace!("Opening input file...");

            let file = File::open(input).map_err(DecodingError::FailedToOpenZipFile)?;

            trace!("Opening ZIP archive...");

            let mut zip = ZipArchive::new(file).map_err(DecodingError::InvalidZipArchive)?;

            let zip_files = zip.len();

            /// Represent a page that has been extracted from the comic archive
            struct ExtractedFile {
                path_in_zip: PathBuf,
                extracted_path: PathBuf,
                extension: Option<String>,
            }

            // List of extracted pages
            let mut pages: Vec<ExtractedFile> = vec![];

            for i in 0..zip.len() {
                trace!("Retrieving ZIP file with ID {}...", i);

                // Get a file from the ZIP
                let mut file = zip.by_index(i).map_err(DecodingError::ZipError)?;

                // Ignore folders
                if file.is_file() {
                    let file_name = file.sanitized_name();

                    // Ensure the file is an image if only images have to be extracted
                    if dec.extract_images_only
                        && !deter::has_image_ext(&file_name, dec.accept_extended_image_formats)
                    {
                        trace!("Ignoring file {}/{} based on extension", i + 1, zip_files);
                        continue;
                    }

                    // Get the file's extension to determine output file's name
                    let ext = file_name
                        .extension()
                        .map(|ext| {
                            ext.to_str()
                                .ok_or_else(|| DecodingError::ZipFileHasInvalidUTF8FileExtension(
                                    file_name.clone(),
                                ))
                        })
                        .transpose()?;

                    let outpath = output.join(Path::new(&format!("___tmp_pic_{}", pages.len())));

                    // Create output file
                    trace!("File is a page. Creating an output file for it...");
                    let mut outfile = File::create(&outpath).map_err(|err| {
                        DecodingError::FailedToCreateOutputFile(err, outpath.clone())
                    })?;

                    // Extract the page
                    debug!("Extracting file {} out of {}...", i + 1, zip_files);
                    io::copy(&mut file, &mut outfile).map_err(|err| {
                        DecodingError::FailedToExtractZipFile {
                            path_in_zip: file_name.clone(),
                            extract_to: outpath.clone(),
                            err,
                        }
                    })?;

                    pages.push(ExtractedFile {
                        extension: ext.map(|ext| ext.to_owned()),
                        path_in_zip: file_name,
                        extracted_path: outpath,
                    });
                }
            }

            trace!("Sorting pages...");

            if dec.simple_sorting {
                pages.sort_by(|a, b| a.path_in_zip.cmp(&b.path_in_zip));
            } else {
                pages.sort_by(|a, b| deter::natural_paths_cmp(&a.path_in_zip, &b.path_in_zip));
            }

            let total_pages = pages.len();

            let mut extracted = vec![];

            // Get the number of characters the last page takes to display
            let page_num_len = pages.len().to_string().len();

            debug!("Renaming pictures...");

            for (i, page) in pages.into_iter().enumerate() {
                let target = output.join(&match page.extension {
                    None => format!("{:0page_num_len$}", i + 1, page_num_len = page_num_len),
                    Some(ref ext) => format!(
                        "{:0page_num_len$}.{}",
                        i + 1,
                        ext,
                        page_num_len = page_num_len
                    ),
                });

                trace!("Renaming picture {}/{}...", i + 1, total_pages);

                fs::rename(&page.extracted_path, &target).map_err(|err| {
                    DecodingError::FailedToRenameTemporaryFile {
                        from: page.extracted_path,
                        to: target.to_owned(),
                        err,
                    }
                })?;

                extracted.push(target);
            }

            Ok(extracted)
        }

        "pdf" => {
            debug!("Matched input format: PDF");
            trace!("Opening input file...");

            let pdf = PDFFile::open(input).map_err(DecodingError::FailedToOpenPdfFile)?;

            let mut images = vec![];

            debug!("Looking for images in the provided PDF...");

            // List all images in the PDF
            for (i, page) in pdf.pages().enumerate() {
                trace!("Counting images from page {}...", i);

                match page.map_err(|err| DecodingError::FailedToGetPdfPage(i + 1, err)) {
                    Err(err) if dec.skip_bad_pdf_pages => warn!("{}", err),
                    Err(err) => return Err(err),
                    Ok(page) => match page
                        .resources()
                        .map_err(|err| DecodingError::FailedToGetPdfPageResources(i + 1, err))
                    {
                        Err(err) if dec.skip_bad_pdf_pages => warn!("{}", err),
                        Err(err) => return Err(err),
                        Ok(resources) => {
                            images.extend(resources.xobjects.iter().filter_map(|(_, &o)| {
                                let xobj = pdf.get(o).ok()?;
                                match *xobj {
                                    XObject::Image(ref im) => Some(xobj),
                                    _ => None,
                                }
                            }));
                        }
                    },
                }
            }

            info!("Extracting {} images from PDF...", images.len());

            let mut extracted = vec![];
            let page_num_len = images.len().to_string().len();

            // Extract all images from the PDF
            for (i, image) in images.iter().enumerate() {
                let image = match **image {
                    XObject::Image(ref im) => im,
                    _ => continue,
                };

                let outpath = output.join(Path::new(&format!(
                    "{:0page_num_len$}.jpg",
                    i + 1,
                    page_num_len = page_num_len
                )));

                debug!("Extracting page {}/{}...", i + 1, images.len());

                fs::write(&outpath, image.as_jpeg().unwrap()).map_err(|err| {
                    DecodingError::FailedToExtractPdfImage(i + 1, outpath.clone(), err)
                })?;

                extracted.push(outpath);
            }

            Ok(extracted)
        }

        _ => {
            if deter::is_supported_for_decoding(ext) {
                warn!("Internal error: format '{}' cannot be handled but is marked as supported nonetheless", ext);
            }

            Err(DecodingError::UnsupportedFormat(ext.to_owned()))
        }
    };

    if let Ok(pages) = &result {
        let elapsed = extraction_started.elapsed();
        info!(
            "Successfully extracted {} pages in {}.{:03} s!",
            pages.len(),
            elapsed.as_secs(),
            elapsed.subsec_millis()
        );
    }

    result
}
