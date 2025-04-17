// diskviz.rs — Single‑file edition
// ============================================================
// Cargo.toml (必須依存)
// [dependencies]
// eframe           = "0.27"
// walkdir          = "2"
// rayon            = "1"
// crossbeam-channel = "0.5"
// rfd              = "0.14"
// open             = "5"
// serde            = { version = "1", features = ["derive"] }
// serde_json       = "1"
// ============================================================

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use crossbeam_channel::{unbounded, Receiver};
use eframe::{egui, egui::{Color32, FontFamily, Id}};
use eframe::egui::{Memory, PopupCloseBehavior};
use rayon::prelude::*;
use walkdir::WalkDir;

// --------------------------- データモデル ---------------------------
#[derive(Default)]
struct DirNode {
    name: Arc<str>,
    size: u64,
    children: Vec<Box<DirNode>>, // 空ならファイル or 未展開
}

// --------------------------- スキャン結果メッセージ ---------------------------
struct ScanProgress {
    total_dirs: u64,
    total_bytes: u64,
    scanned_dirs: u64,
    scanned_bytes: u64,
}

enum ScanMsg {
    Progress(ScanProgress),
    Finished(Box<DirNode>),
}

// --------------------------- 走査関数 (1‑Pass / Rayon) ---------------------------
fn spawn_scan(path: PathBuf) -> Receiver<ScanMsg> {
    let (s, r) = unbounded();
    std::thread::spawn(move || {
        println!("Start scanning: {}", path.display());

        // First: collect entries in parallel and build tree
        let entries: Vec<_> = WalkDir::new(&path)
            .into_iter()
            .par_bridge() // rayon parallel
            .filter_map(Result::ok)
            .collect();

        let total_dirs = entries.iter().filter(|e| e.file_type().is_dir()).count() as u64;
        let total_bytes: u64 = entries
            .par_iter()
            .filter(|e| e.file_type().is_file())
            .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
            .sum();

        let mut root = Box::new(DirNode::default());
        root.name = path.to_string_lossy().into();
        root.size = total_bytes;

        // Map from parent path to children list
        let mut map: HashMap<PathBuf, Vec<(Arc<str>, u64)>> = HashMap::new();
        for entry in &entries {
            let meta = entry.metadata().ok();
            if let Some(m) = meta {
                if m.is_file() {
                    let parent = entry.path().parent().unwrap_or(&path);
                    map.entry(parent.to_path_buf())
                        .or_default()
                        .push((entry.file_name().to_string_lossy().into(), m.len()));
                }
            }
        }
        // Attach to tree recursively (depth 1 for MVP)
        for (p, list) in map.into_iter() {
            let mut dir = Box::new(DirNode::default());
            dir.name = p.file_name().unwrap_or_default().to_string_lossy().into();
            dir.size = list.iter().map(|(_, sz)| *sz).sum();
            dir.children = list
                .into_iter()
                .map(|(n, sz)| Box::new(DirNode { name: n, size: sz, children: vec![] }))
                .collect();
            root.children.push(dir);
        }

        s.send(ScanMsg::Progress(ScanProgress {
            total_dirs,
            total_bytes,
            scanned_dirs: total_dirs,
            scanned_bytes: total_bytes,
        }))
            .ok();
        s.send(ScanMsg::Finished(root)).ok();
        println!("Scan finished.");
    });
    r
}

// --------------------------- egui アプリ ---------------------------
struct DiskVizApp {
    tree: Option<Box<DirNode>>,
    rx: Option<Receiver<ScanMsg>>,
    progress: Option<ScanProgress>,
    bread: Vec<*const DirNode>,
}

impl Default for DiskVizApp {
    fn default() -> Self {
        Self { tree: None, rx: None, progress: None, bread: Vec::new() }
    }
}

impl eframe::App for DiskVizApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            if ui.button("Select & Scan").clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    self.rx = Some(spawn_scan(p));
                    self.progress = None;
                    self.tree = None;
                    self.bread.clear();
                }
            }
            if let Some(prog) = &self.progress {
                ui.add(egui::ProgressBar::new(prog.scanned_bytes as f32 / prog.total_bytes.max(1) as f32)
                    .text(format!("{} / {} MB", prog.scanned_bytes / 1_048_576, prog.total_bytes / 1_048_576)));
            }
        });

        // Handle messages
        loop {
            // ───────────── borrow をこの小さなブロック内だけに閉じ込め ─────────────
            let msg = {
                // self.rx を一時的に不変借用
                let rx_ref = match &self.rx {
                    Some(rx) => rx,
                    None     => break,   // チャンネルがなければループ終了
                };
                // メッセージが取れたら返し、Empty/Disconnected ならループ終了
                match rx_ref.try_recv() {
                    Ok(msg)      => msg,
                    Err(_)       => break,
                }
            };
            // ─── borrow がここで終了 ───

            // ここからは自由に self.rx = None できます
            match msg {
                ScanMsg::Progress(p) => {
                    self.progress = Some(p);
                }
                ScanMsg::Finished(root) => {
                    self.tree = Some(root);
                    self.rx = None;  // 借用がもう残っていないので OK
                }
            }
        }


        // Show treemap simple list
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(root) = &self.tree {
                ui.heading("Top‑level items");
                for child in &root.children {
                    let pct = child.size as f64 / root.size as f64 * 100.0;
                    let selectable = ui.selectable_label(false, format!("{:<10} {:>8.2}%", child.name, pct));
                    if selectable.clicked() {
                        // No deep zoom implemented in this minimal snippet
                    }
                    if selectable.secondary_clicked() {
                        ui.memory_mut(| memory : &mut Memory| {memory.toggle_popup(Id::new(&*child.name))})
                    }
                    egui::popup::popup_above_or_below_widget(
                        ui,
                        Id::new(&*child.name),
                        &selectable,
                        egui::AboveOrBelow::Below,
                        PopupCloseBehavior::CloseOnClickOutside, 
                        |ui| {
                            if ui.button("Copy Path").clicked() {
                                ui.output_mut(|o| o.copied_text = child.name.to_string());
                            }
                            if ui.button("Open in File Manager").clicked() {
                                let _ = open::that(Path::new(&*child.name));
                            } 
                        });
                    let bar_width = ui.available_width();
                    let (rect, _response) = ui.allocate_space(egui::vec2(bar_width * pct as f32 / 100.0, 8.0));
                    ui.painter().rect_filled(
                        _response,
                        0.0,
                        Color32::LIGHT_BLUE,
                    );
                }
            } else {
                ui.label("No data yet…");
            }
        });

        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

fn setup_jp_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    // NotoSansJP が assets フォルダにある想定。無ければデフォルトで動く。
    if let Ok(bytes) = std::fs::read("assets/NotoSansJP-VariableFont_wght.ttf") {
        fonts.font_data.insert(
            "noto".into(),
            egui::FontData::from_owned(bytes).into(),
        );
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "noto".into());
    }
    ctx.set_fonts(fonts);
}

// --------------------------- entry ---------------------------
fn main() {
    rayon::ThreadPoolBuilder::new().build_global().unwrap();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 700.0]),
        ..Default::default()
    };

    eframe::run_native(
        "DiskViz (single‑file MVP)",
        options,
        Box::new(|cc | {
            setup_jp_fonts(&cc.egui_ctx);
            Ok(Box::new(DiskVizApp::default()))
        }),
    );
}
