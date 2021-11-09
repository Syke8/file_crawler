use std::{
    collections::HashSet,
    env,
    fs::{self, File},
    hash::{Hash, Hasher},
    io::{stdin, ErrorKind, Write},
    path::Path,
    time::Instant,
};

use async_std::task;
use chrono::Local;
use futures::{
    channel::mpsc::{self, UnboundedReceiver, UnboundedSender},
    future::join_all,
    StreamExt,
};
use serde::{Deserialize, Serialize};
use serde_json::{map::Entry, Value};

const TOOL_REVISION: u32 = 1;

static mut TRACE_LOG: bool = false;
static mut SLOW_MODE: bool = false;
static mut MANUAL_MODE: bool = false;
static mut LOGGING: bool = false;

fn tracing_enabled() -> bool {
    unsafe { TRACE_LOG }
}

fn slow_mode_enabled() -> bool {
    unsafe { SLOW_MODE }
}

fn manual_mode() -> bool {
    unsafe { MANUAL_MODE }
}

fn log_enabled() -> bool {
    unsafe { LOGGING }
}

fn read_args() -> Option<()> {
    unsafe {
        for arg in env::args() {
            match arg.as_str() {
                "-t" | "--trace" => TRACE_LOG = true,
                "-s" | "--slow-mode" => SLOW_MODE = true,
                "-m" | "--manual" => MANUAL_MODE = true,
                "-l" | "--log" => LOGGING = true,
                "-h" | "--help" => return None,
                _ => continue,
            }
        }
    }

    Some(())
}

#[async_std::main]
async fn main() {
    if let None = read_args() {
        println!("-t or --trace to print what the tool is doing");
        println!("-s or --slow-mode is useful if you have a slow cpu/hard drive so the tool won't take all the ressources available but of course will be slower");
        println!("-m or --manual analyses the folders specified inside folders.json instead of the most common folders used by applications/installers");
        println!("-h or --help you just used it");

        return;
    }

    let start_instant = Instant::now();
    compare_analysis(
        "record_2021-11-07_01-47-59.json",
        "record_2021-11-07_01-48-00.json",
    )
    .await;
    println!("Time : {}ms", start_instant.elapsed().as_millis());
    return;

    let folders: HashSet<String>;

    if manual_mode() {
        println!("Manual scan");

        folders = load_manual_mode_folders();

        if folders.len() == 0 {
            println!("No folders specified");

            return;
        }
    } else {
        println!("Auto scan");

        folders = load_auto_mode_folders();
    }

    let start_instant = Instant::now();

    let (tx, rx) = mpsc::unbounded::<RecorderSignal>();

    let recorder_work = task::spawn(file_recorder(rx, folders.len()));

    if slow_mode_enabled() {
        println!("Slow mode");

        for path in folders {
            read_path(path, tx.clone()).await;
        }
    } else {
        println!("Fast mode");

        join_all(
            folders
                .into_iter()
                .map(|path| task::spawn(read_path(path, tx.clone()))),
        )
        .await;
    }
    let _ = tx.unbounded_send(RecorderSignal::Close);
    recorder_work.await;

    println!("Time : {}ms", start_instant.elapsed().as_millis());

    println!("Press Enter to exit");
    let mut buff = String::new();
    let _ = stdin().read_line(&mut buff);
}

fn load_manual_mode_folders() -> HashSet<String> {
    let target_folders: Value =
        serde_json::from_str(&fs::read_to_string("folders.json").expect("JSON file doesn't exist"))
            .expect("Malformated JSON");

    HashSet::from_iter(
        target_folders["folders"]
            .as_array()
            .expect("No folders found in JSON")
            .iter()
            .map(|v| v.to_string()),
    )
}

fn load_auto_mode_folders() -> HashSet<String> {
    let system_dir = format!(
        "{}\\",
        env::var("SYSTEMDRIVE").expect("Cannot get System Dir")
    );
    let program_data = env::var("ProgramData").expect("Cannot get Program Data");
    let program_files = env::var("ProgramFiles").expect("Cannot get Program Files");
    let program_files_x86 = env::var("ProgramFiles(x86)").expect("Cannot get Program Files x86");
    let user_profile = env::var("USERPROFILE").expect("Cannot get User Profile");
    let app_data = env::var("APPDATA").expect("Cannot get Roaming AppData");
    let local_app_data = env::var("LOCALAPPDATA").expect("Cannot get Local AppData");
    let local_low_app_data = format!("{}\\AppData\\LocalLow", user_profile);

    HashSet::from([
        system_dir,
        program_data,
        program_files,
        program_files_x86,
        user_profile,
        app_data,
        local_app_data,
        local_low_app_data,
    ])
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Hash)]
enum EntryType {
    File,
    Directory,
    Unknown,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct EntryInfo {
    #[serde(rename = "Type")]
    entry_type: EntryType,
    #[serde(rename = "Path")]
    path: String,
    #[serde(rename = "Octets")]
    #[serde(skip_serializing_if = "octets_is_zero")]
    octets: u64,
}

impl Hash for EntryInfo {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.entry_type.hash(state);
        self.path.hash(state);
        self.octets.hash(state);
    }
}

#[derive(Serialize, Deserialize, Eq)]
struct Crawl {
    #[serde(rename = "DateTime")]
    date_time: String,
    #[serde(rename = "ToolRevision")]
    used_tool_revision: u32, // for compatibility in the long run
    #[serde(rename = "EntryCount")]
    entry_count: usize,
    #[serde(rename = "Entries")]
    entries_info: HashSet<EntryInfo>,
}

impl PartialEq for Crawl {
    // Don't take in account the datetime because it's purely for information to the user
    fn eq(&self, other: &Self) -> bool {
        self.used_tool_revision == other.used_tool_revision
            && self.entry_count == other.entry_count
            && self.entries_info == other.entries_info
    }
}

enum RecorderSignal {
    EntriesVec(Box<Vec<EntryInfo>>),
    Close,
}

#[derive(Serialize)]
enum EntryDifferenceType {
    New,
    Removed,
    SizeChange,
    NoChange,
}

#[derive(Serialize)]
struct EntryDifference<'a> {
    #[serde(rename = "Type")]
    entry_type: &'a EntryType,
    #[serde(rename = "DifferenceType")]
    difference_type: EntryDifferenceType,
    #[serde(rename = "Path")]
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<&'a str>, // None if difference_type == Removed
    #[serde(rename = "OctetsDifference")]
    #[serde(skip_serializing_if = "octets_is_zero")]
    octets_difference: u64,
}

fn octets_is_zero(diff: &u64) -> bool {
    *diff == 0
}

#[derive(Serialize)]
struct DifferenceAnalysis<'a> {
    #[serde(rename = "DateTime")]
    date_time: String,
    #[serde(rename = "EntriesDifference")]
    entries_difference: Vec<EntryDifference<'a>>,
}

async fn compare_analysis(first_file: &str, second_file: &str) {
    let first_analysis =
        serde_json::from_slice::<Crawl>(fs::read(first_file).unwrap().as_slice()).unwrap();

    let second_analysis =
        serde_json::from_slice::<Crawl>(fs::read(second_file).unwrap().as_slice()).unwrap();

    // get the dates and compare the oldest it with the newest

    // const core_count: usize = 24;

    // if first_analysis.entry_count >= core_count {
    //     let sub_task_count = first_analysis.entry_count / core_count;
    //     let sub_task_count_rest = first_analysis.entry_count % core_count;

    //     let mut tasks = Vec::with_capacity(core_count);

    //     let entries_vec: Vec<EntryInfo> = first_analysis.entries_info.into_iter().collect();

    //     let mut step = 0usize;

    //     for core in 0..core_count {
    //         let range: usize;

    //         if core == core_count - 1 {
    //             range = sub_task_count + sub_task_count_rest;
    //         } else {
    //             range = sub_task_count;
    //         }

    //         let sub_tasks = entries_vec[step..step + range].to_vec();

    //         tasks.push(task::spawn(async move { for entry in sub_tasks {} }));

    //         step += range;
    //     }

    //     join_all(tasks).await;
    // }

    let entries_not_changed = first_analysis
        .entries_info
        .intersection(&second_analysis.entries_info);

    let entries_changed = first_analysis
        .entries_info
        .difference(&second_analysis.entries_info);

    let mut difference_analysis = Vec::new();

    for entry in entries_not_changed {
        difference_analysis.push(EntryDifference {
            entry_type: &entry.entry_type,
            difference_type: EntryDifferenceType::NoChange,
            path: Some(&entry.path),
            octets_difference: 0,
        });
    }

    for entry in entries_changed {
        difference_analysis.push(EntryDifference {
            entry_type: &entry.entry_type,
            difference_type: EntryDifferenceType::New,
            path: Some(&entry.path),
            octets_difference: entry.octets,
        });
    }

    let date_time = Local::now().format("%F_%H-%M-%S").to_string();

    let mut analysis_file = match File::create(format!("analysis_{}.json", &date_time)) {
        Ok(file) => file,
        Err(_) => {
            println!("Couldn't create the result file");
            return;
        }
    };

    let analysis = DifferenceAnalysis {
        date_time,
        entries_difference: difference_analysis,
    };

    if let Ok(analysis_json) = serde_json::to_string_pretty(&analysis) {
        if let Err(_) = analysis_file.write_all(analysis_json.as_bytes()) {
            println!("Couldn't write into analysis file");
        }
    }
}

async fn file_recorder(mut receiver: UnboundedReceiver<RecorderSignal>, jobs_working: usize) {
    let mut jobs_done = 0usize;

    let mut entries_info: HashSet<EntryInfo> = HashSet::new();

    while let Some(signal) = receiver.next().await {
        match signal {
            RecorderSignal::EntriesVec(entries) => {
                for entry in entries.into_iter() {
                    entries_info.insert(entry);
                }

                jobs_done += 1;

                if tracing_enabled() {
                    println!("{} out of {} job done", jobs_done, jobs_working);
                }

                if jobs_done == jobs_working {
                    break;
                }
            }
            RecorderSignal::Close => receiver.close(),
        }
    }

    if tracing_enabled() {
        println!("All jobs done droping the receiver");
    }
    drop(receiver);

    let date_time = Local::now().format("%F_%H-%M-%S").to_string();

    let mut crawl_file = match File::create(format!("record_{}.json", &date_time)) {
        Ok(file) => file,
        Err(_) => {
            println!("Couldn't create the result file");
            return;
        }
    };

    let crawl = Crawl {
        date_time,
        used_tool_revision: TOOL_REVISION,
        entry_count: entries_info.len(),
        entries_info,
    };

    if let Ok(crawl_json) = serde_json::to_string_pretty(&crawl) {
        if let Err(_) = crawl_file.write_all(crawl_json.as_bytes()) {
            println!("Couldn't write into record file");
        }
    }
}

async fn read_path(path: String, sender: UnboundedSender<RecorderSignal>) {
    if log_enabled() {
        println!("Analysing {}", path);
    }

    let entries = match fs::read_dir(Path::new(&path)) {
        Ok(entries) => entries,
        Err(err) => {
            match err.kind() {
                ErrorKind::PermissionDenied => {
                    println!("Permission Denied : {}", path)
                }
                _ => println!("Error {} : {}", err.to_string(), path),
            }

            return;
        }
    };

    let mut entries_info: Box<Vec<EntryInfo>> = Box::new(Vec::new());

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };

        if !entry.path().exists() {
            continue;
        }

        let entry_type: EntryType;

        if entry.path().is_file() {
            entry_type = EntryType::File;

            if log_enabled() {
                println!("FILE: {}", entry.path().display());
            }
        } else if entry.path().is_dir() {
            entry_type = EntryType::Directory;

            if log_enabled() {
                println!("DIRECTORY: {}", entry.path().display());
            }
        } else {
            entry_type = EntryType::Unknown;

            if log_enabled() {
                println!("UNKNOWN: {}", entry.path().display());
            }
        }

        let path = entry.path().to_str().unwrap_or("Invalid path").to_owned();
        let octets: u64 = match entry.metadata() {
            Ok(metadata) => metadata.len(),
            Err(_) => 0,
        };

        let ent = EntryInfo {
            entry_type,
            path,
            octets,
        };

        entries_info.push(ent);
    }

    if tracing_enabled() {
        println!("Sending {} SIGNAL", path);
    }

    if let Err(_) = sender.unbounded_send(RecorderSignal::EntriesVec(entries_info)) {
        println!(
            "Couldn't process due to system error (make sure you have enough memory available)"
        );
    }

    drop(sender);
}
