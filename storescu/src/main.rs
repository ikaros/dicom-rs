use dicom_core::dicom_value;
use dicom_core::smallvec;
use dicom_core::{header::Tag, DataElement, PrimitiveValue, VR};
use dicom_dictionary_std::tags;
use dicom_encoding::transfer_syntax;
use dicom_object::{mem::InMemDicomObject, open_file, StandardDataDictionary};
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use dicom_ul::pdu::Pdu;
use dicom_ul::{
    association::ClientAssociationOptions,
    pdu::{PDataValue, PDataValueType},
};
use indicatif::{ProgressBar, ProgressStyle};
use smallvec::smallvec;
use snafu::{prelude::*, ErrorCompat};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use structopt::StructOpt;
use transfer_syntax::TransferSyntaxIndex;
use walkdir::WalkDir;

/// DICOM C-STORE SCU
#[derive(Debug, StructOpt)]
struct App {
    /// socket address to STORE SCP (example: "127.0.0.1:104")
    addr: String,
    /// the DICOM file(s) to store
    files: Vec<PathBuf>,
    /// verbose mode
    #[structopt(short = "v", long = "verbose")]
    verbose: bool,
    /// the C-STORE message ID
    #[structopt(short = "m", long = "message-id", default_value = "1")]
    message_id: u16,
    /// the calling AE title
    #[structopt(long = "calling-ae-title", default_value = "STORE-SCU")]
    calling_ae_title: String,
    /// the called AE title
    #[structopt(long = "called-ae-title", default_value = "ANY-SCP")]
    called_ae_title: String,
    /// the maximum PDU length accepted by the SCU
    #[structopt(long = "max-pdu-length", default_value = "16384")]
    max_pdu_length: u32,
    /// fail if not all DICOM files can be transferred
    #[structopt(long = "fail-first")]
    fail_first: bool,
}

struct DicomFile {
    /// File path
    file: PathBuf,
    /// Storage SOP Class UID
    sop_class_uid: String,
    /// Storage SOP Instance UID
    sop_instance_uid: String,
    /// File Transfer Syntax
    file_transfer_syntax: String,
    /// Transfer Syntax selected
    ts_selected: Option<String>,
    /// Presentation Context selected
    pc_selected: Option<dicom_ul::pdu::PresentationContextResult>,
}

fn report<E: 'static>(err: &E)
where
    E: std::error::Error,
{
    eprintln!("[ERROR] {}", err);
    if let Some(source) = err.source() {
        eprintln!();
        eprintln!("Caused by:");
        for (i, e) in std::iter::successors(Some(source), |e| e.source()).enumerate() {
            eprintln!("   {}: {}", i, e);
        }
    }
}

fn report_backtrace<E: 'static>(err: &E)
where
    E: std::error::Error,
    E: ErrorCompat,
{
    let env_backtrace = std::env::var("RUST_BACKTRACE").unwrap_or_default();
    let env_lib_backtrace = std::env::var("RUST_LIB_BACKTRACE").unwrap_or_default();
    if env_lib_backtrace == "1" || (env_backtrace == "1" && env_lib_backtrace != "0") {
        if let Some(backtrace) = ErrorCompat::backtrace(&err) {
            eprintln!();
            eprintln!("Backtrace:");
            eprintln!("{}", backtrace);
        }
    }
}

fn report_with_backtrace<E: 'static>(err: E)
where
    E: std::error::Error,
    E: ErrorCompat,
{
    report(&err);
    report_backtrace(&err);
}

#[derive(Debug, Snafu)]
enum Error {
    /// Could not initialize SCU
    InitScu {
        source: dicom_ul::association::client::Error,
    },

    /// Could not construct DICOM command
    CreateCommand { source: dicom_object::Error },

    #[snafu(whatever, display("{}", message))]
    Other {
        message: String,
        #[snafu(source(from(Box<dyn std::error::Error + 'static>, Some)))]
        source: Option<Box<dyn std::error::Error + 'static>>,
    },
}

fn main() {
    run().unwrap_or_else(|e| {
        report_with_backtrace(e);
        std::process::exit(-2);
    });
}

fn run() -> Result<(), Error> {
    tracing::subscriber::set_global_default(tracing_subscriber::FmtSubscriber::new())
        .unwrap_or_else(|e| {
            report(&e);
        });

    let App {
        addr,
        files,
        verbose,
        message_id,
        calling_ae_title,
        called_ae_title,
        max_pdu_length,
        fail_first,
    } = App::from_args();

    let mut checked_files: Vec<PathBuf> = vec![];
    let mut dicom_files: Vec<DicomFile> = vec![];
    let mut presentation_contexts = HashSet::new();

    for file in files {
        if file.is_dir() {
            for file in WalkDir::new(file.as_path())
                .into_iter()
                .filter_map(Result::ok)
                .filter(|f| !f.file_type().is_dir())
            {
                checked_files.push(file.into_path());
            }
        } else {
            checked_files.push(file);
        }
    }

    for file in checked_files {
        if verbose {
            println!("Opening file '{}'...", file.display());
        }

        match check_file(&file) {
            Ok(dicom_file) => {
                presentation_contexts.insert((
                    dicom_file.sop_class_uid.to_string(),
                    dicom_file.file_transfer_syntax.clone(),
                ));
                dicom_files.push(dicom_file);
            }
            Err(e) => {
                if verbose {
                    println!("Could not open file {}: {}", file.display(), e);
                }
            }
        }
    }

    if dicom_files.is_empty() {
        eprintln!("No supported files to transfer");
        std::process::exit(-1);
    }

    if verbose {
        println!("Establishing association with '{}'...", &addr);
    }

    let mut scu_init = ClientAssociationOptions::new();

    for (storage_sop_class_uid, transfer_syntax) in &presentation_contexts {
        scu_init = scu_init.with_presentation_context(storage_sop_class_uid, vec![transfer_syntax]);
    }
    let mut scu = scu_init
        .calling_ae_title(calling_ae_title)
        .called_ae_title(called_ae_title)
        .max_pdu_length(max_pdu_length)
        .establish(addr)
        .context(InitScuSnafu)?;

    if verbose {
        println!("Association established");
    }

    for mut file in &mut dicom_files {
        // TODO(#106) transfer syntax conversion is currently not supported
        let r: Result<_, Error> = check_presentation_contexts(file, scu.presentation_contexts())
            .whatever_context::<_, _>("Could not choose a transfer syntax");
        match r {
            Ok((pc, ts)) => {
                file.pc_selected = Some(pc);
                file.ts_selected = Some(ts);
            }
            Err(e) => {
                report(&e);
                if fail_first {
                    let _ = scu.abort();
                    std::process::exit(-2);
                }
            }
        }
    }

    let progress_bar;
    if !verbose {
        progress_bar = Some(ProgressBar::new(dicom_files.len() as u64));
        if let Some(pb) = progress_bar.as_ref() {
            pb.set_style(
                ProgressStyle::default_bar()
                    .template("[{elapsed_precise}] {bar:40} {pos}/{len} {msg}"),
            )
        };
    } else {
        progress_bar = None;
    }

    for file in dicom_files {
        if let (Some(pc_selected), Some(ts_selected)) = (file.pc_selected, file.ts_selected) {
            let cmd = store_req_command(&file.sop_class_uid, &file.sop_instance_uid, message_id);

            let mut cmd_data = Vec::with_capacity(128);
            cmd.write_dataset_with_ts(
                &mut cmd_data,
                &dicom_transfer_syntax_registry::entries::IMPLICIT_VR_LITTLE_ENDIAN.erased(),
            )
            .context(CreateCommandSnafu)?;

            let mut object_data = Vec::with_capacity(2048);
            let dicom_file =
                open_file(file.file).whatever_context("Could not open listed DICOM file")?;
            let ts_selected = TransferSyntaxRegistry
                .get(&ts_selected)
                .whatever_context("Unsupported file transfer syntax")?;
            dicom_file
                .write_dataset_with_ts(&mut object_data, ts_selected)
                .whatever_context("Could not write object dataset")?;

            let nbytes = cmd_data.len() + object_data.len();

            if verbose {
                println!("Sending payload (~ {} kB)...", nbytes / 1_000);
            }

            if nbytes < scu.acceptor_max_pdu_length() as usize - 100 {
                let pdu = Pdu::PData {
                    data: vec![
                        PDataValue {
                            presentation_context_id: pc_selected.id,
                            value_type: PDataValueType::Command,
                            is_last: true,
                            data: cmd_data,
                        },
                        PDataValue {
                            presentation_context_id: pc_selected.id,
                            value_type: PDataValueType::Data,
                            is_last: true,
                            data: object_data,
                        },
                    ],
                };

                scu.send(&pdu)
                    .whatever_context("Failed to send C-STORE-RQ")?;
            } else {
                let pdu = Pdu::PData {
                    data: vec![PDataValue {
                        presentation_context_id: pc_selected.id,
                        value_type: PDataValueType::Command,
                        is_last: true,
                        data: cmd_data,
                    }],
                };

                scu.send(&pdu)
                    .whatever_context("Failed to send C-STORE-RQ command")?;

                {
                    let mut pdata = scu.send_pdata(pc_selected.id);
                    pdata
                        .write_all(&object_data)
                        .whatever_context("Failed to send C-STORE-RQ P-Data")?;
                }
            }

            if verbose {
                println!("Awaiting response...");
            }

            let rsp_pdu = scu
                .receive()
                .whatever_context("Failed to receive C-STORE-RSP")?;

            match rsp_pdu {
                Pdu::PData { data } => {
                    let data_value = &data[0];

                    let cmd_obj = InMemDicomObject::read_dataset_with_ts(
                        &data_value.data[..],
                        &dicom_transfer_syntax_registry::entries::IMPLICIT_VR_LITTLE_ENDIAN
                            .erased(),
                    )
                    .whatever_context("Could not read response from SCP")?;
                    if verbose {
                        println!("Response: {:?}", cmd_obj);
                    }
                    let status = cmd_obj
                        .element(tags::STATUS)
                        .whatever_context("Could not find status code in response")?
                        .to_int::<u16>()
                        .whatever_context("Status code in response is not a valid integer")?;
                    let storage_sop_instance_uid = file
                        .sop_instance_uid
                        .trim_end_matches(|c: char| c.is_whitespace() || c == '\0');
                    if status == 0 {
                        if verbose {
                            println!(
                                "Successfully stored instance `{}`",
                                storage_sop_instance_uid
                            );
                        }
                    } else {
                        println!(
                            "Failed to store instance `{}` (status code {})",
                            storage_sop_instance_uid, status
                        );
                        if fail_first {
                            let _ = scu.abort();
                            std::process::exit(-2);
                        }
                    }
                }

                pdu @ Pdu::Unknown { .. }
                | pdu @ Pdu::AssociationRQ { .. }
                | pdu @ Pdu::AssociationAC { .. }
                | pdu @ Pdu::AssociationRJ { .. }
                | pdu @ Pdu::ReleaseRQ
                | pdu @ Pdu::ReleaseRP
                | pdu @ Pdu::AbortRQ { .. } => {
                    eprintln!("Unexpected SCP response: {:?}", pdu);
                    let _ = scu.abort();
                    std::process::exit(-2);
                }
            }
        }
        if let Some(pb) = progress_bar.as_ref() {
            pb.inc(1)
        };
    }

    if let Some(pb) = progress_bar {
        pb.finish_with_message("done")
    };

    scu.release()
        .whatever_context("Failed to release SCU association")?;
    Ok(())
}

fn store_req_command(
    storage_sop_class_uid: &str,
    storage_sop_instance_uid: &str,
    message_id: u16,
) -> InMemDicomObject<StandardDataDictionary> {
    let mut obj = InMemDicomObject::new_empty();

    // group length
    obj.put(DataElement::new(
        tags::COMMAND_GROUP_LENGTH,
        VR::UL,
        PrimitiveValue::from(
            12 + 8
                + even_len(storage_sop_class_uid.len())
                + 10
                + 10
                + 10
                + 10
                + 8
                + even_len(storage_sop_instance_uid.len()),
        ),
    ));

    // SOP Class UID
    obj.put(DataElement::new(
        tags::AFFECTED_SOP_CLASS_UID,
        VR::UI,
        dicom_value!(Str, storage_sop_class_uid),
    ));

    // command field
    obj.put(DataElement::new(
        tags::COMMAND_FIELD,
        VR::US,
        dicom_value!(U16, [0x0001]),
    ));

    // message ID
    obj.put(DataElement::new(
        tags::MESSAGE_ID,
        VR::US,
        dicom_value!(U16, [message_id]),
    ));

    //priority
    obj.put(DataElement::new(
        tags::PRIORITY,
        VR::US,
        dicom_value!(U16, [0x0000]),
    ));

    // data set type
    obj.put(DataElement::new(
        tags::COMMAND_DATA_SET_TYPE,
        VR::US,
        dicom_value!(U16, [0x0000]),
    ));

    // affected SOP Instance UID
    obj.put(DataElement::new(
        tags::AFFECTED_SOP_INSTANCE_UID,
        VR::UI,
        dicom_value!(Str, storage_sop_instance_uid),
    ));

    obj
}

fn even_len(l: usize) -> u32 {
    ((l + 1) & !1) as u32
}

fn check_file(file: &Path) -> Result<DicomFile, Error> {
    // Ignore DICOMDIR files until better support is added
    let _ = (file.file_name() != Some(OsStr::new("DICOMDIR")))
        .then(|| false)
        .whatever_context("DICOMDIR file not supported")?;
    let dicom_file = dicom_object::OpenFileOptions::new()
        .read_until(Tag(0x0001, 0x000))
        .open_file(&file)
        .with_whatever_context(|_| format!("Could not open DICOM file {}", file.display()))?;

    let meta = dicom_file.meta();

    let storage_sop_class_uid = &meta.media_storage_sop_class_uid;
    let storage_sop_instance_uid = &meta.media_storage_sop_instance_uid;
    let transfer_syntax_uid = &meta.transfer_syntax.trim_end_matches('\0');
    let ts = TransferSyntaxRegistry
        .get(transfer_syntax_uid)
        .whatever_context("Unsupported file transfer syntax")?;
    Ok(DicomFile {
        file: file.to_path_buf(),
        sop_class_uid: storage_sop_class_uid.to_string(),
        sop_instance_uid: storage_sop_instance_uid.to_string(),
        file_transfer_syntax: String::from(ts.uid()),
        ts_selected: None,
        pc_selected: None,
    })
}

fn check_presentation_contexts(
    file: &DicomFile,
    pcs: &[dicom_ul::pdu::PresentationContextResult],
) -> Result<(dicom_ul::pdu::PresentationContextResult, String), Error> {
    let file_ts = TransferSyntaxRegistry
        .get(&file.file_transfer_syntax)
        .whatever_context("Unsupported file transfer syntax")?;
    // TODO(#106) transfer syntax conversion is currently not supported
    let pc = pcs
        .iter()
        .find(|pc| {
            // Check support for this transfer syntax.
            // If it is the same as the file, we're good.
            // Otherwise, uncompressed data set encoding
            // and native pixel data is required on both ends.
            let ts = &pc.transfer_syntax;
            ts == file_ts.uid()
                || TransferSyntaxRegistry
                    .get(&pc.transfer_syntax)
                    .filter(|ts| file_ts.is_codec_free() && ts.is_codec_free())
                    .map(|_| true)
                    .unwrap_or(false)
        })
        .whatever_context("No presentation context accepted")?;
    let ts = TransferSyntaxRegistry
        .get(&pc.transfer_syntax)
        .whatever_context("Poorly negotiated transfer syntax")?;

    Ok((pc.clone(), String::from(ts.uid())))
}

#[cfg(test)]
mod tests {
    use super::even_len;
    #[test]
    fn test_even_len() {
        assert_eq!(even_len(0), 0);
        assert_eq!(even_len(1), 2);
        assert_eq!(even_len(2), 2);
        assert_eq!(even_len(3), 4);
        assert_eq!(even_len(4), 4);
        assert_eq!(even_len(5), 6);
    }
}
