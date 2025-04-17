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
    path::{PathBuf},
    sync::Arc,
    time::Duration,
};
use std::process::Command;

use crossbeam_channel::{unbounded, Receiver};
use eframe::{egui, egui::{Color32, Id, Layout, Align}};
use eframe::egui::TextWrapMode;
use egui::{ScrollArea, Memory, FontDefinitions, FontFamily, FontData};
use egui::popup::PopupCloseBehavior;
use rayon::prelude::*;
use rayon::iter::ParallelBridge;
use walkdir::WalkDir;
use rfd::FileDialog;
use open;

// --------------------------- データモデル ---------------------------
#[derive(Debug)]
struct DirNode {
    name: Arc<str>,
    path: PathBuf,
    size: u64,
    children: Vec<Box<DirNode>>,
}

impl DirNode {
    fn new(name: Arc<str>, path: PathBuf, size: u64) -> Self {
        Self { name, path, size, children: Vec::new() }
    }
}

impl Default for DirNode {
    fn default() -> Self {
        Self { name: Arc::from(""), path: PathBuf::new(), size: 0, children: Vec::new() }
    }
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
fn spawn_scan(root_path: PathBuf) -> Receiver<ScanMsg> {
    let (s, r) = unbounded();
    std::thread::spawn(move || {
        // WalkDir でエントリ収集
        let entries: Vec<_> = WalkDir::new(&root_path)
            .into_iter()
            .par_bridge()
            .filter_map(Result::ok)
            .collect();
        // ディレクトリ数とファイル合計バイト数計算
        let total_dirs = entries.iter().filter(|e| e.file_type().is_dir()).count() as u64;
        let total_bytes: u64 = entries.par_iter()
            .filter(|e| e.file_type().is_file())
            .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
            .sum();

        // ルートノード作成
        let mut root = Box::new(DirNode::new(
            Arc::from(root_path.file_name().unwrap_or_default().to_string_lossy().as_ref()),
            root_path.clone(),
            total_bytes,
        ));

        // 親パスごとにファイルを集約
        let mut map: HashMap<PathBuf, Vec<(Arc<str>, u64)>> = HashMap::new();
        for entry in entries.into_iter().filter(|e| e.file_type().is_file()) {
            if let Ok(m) = entry.metadata() {
                let parent = entry.path().parent().unwrap_or(&root_path).to_path_buf();
                let name = Arc::from(entry.file_name().to_string_lossy().as_ref());
                map.entry(parent).or_default().push((name, m.len()));
            }
        }

        // map を元にルートの children を構築
        for (dir_path, list) in map.into_iter() {
            if dir_path == root_path {
                // ルート直下のファイルはそのまま root.children に追加
                for (name, sz) in list {
                    let child_path = root_path.join(&*name);
                    root.children.push(Box::new(DirNode::new(name, child_path, sz)));
                }
            } else {
                // サブディレクトリとして扱う
                let name: Arc<str> = Arc::from(dir_path.file_name().unwrap_or_default().to_string_lossy().as_ref());
                let mut node = Box::new(DirNode::new(
                    name.clone(),
                    dir_path.clone(),
                    list.iter().map(|(_,sz)| *sz).sum(),
                ));
                node.children = list.into_iter().map(|(n, sz)| {
                    let child_path = dir_path.join(&*n);
                    Box::new(DirNode::new(n, child_path, sz))
                }).collect();
                root.children.push(node);
            }
        }

        // メッセージ送信
        s.send(ScanMsg::Progress(ScanProgress { total_dirs, total_bytes, scanned_dirs: total_dirs, scanned_bytes: total_bytes })).ok();
        s.send(ScanMsg::Finished(root)).ok();
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
            if ui.button("ディレクトリ選択してスキャン").clicked() {
                if let Some(path) = FileDialog::new().pick_folder() {
                    self.rx = Some(spawn_scan(path));
                    self.progress = None;
                    self.tree = None;
                    self.bread.clear();
                }
            }
            if let Some(prog) = &self.progress {
                ui.add(egui::ProgressBar::new(
                    prog.scanned_bytes as f32 / (prog.total_bytes.max(1) as f32)
                ).text(format!("{} / {} MB", prog.scanned_bytes/1_048_576, prog.total_bytes/1_048_576)));
            }
        });

        // メッセージ受信
        loop {
            let msg = {
                let rx_ref = match &self.rx { Some(rx) => rx, None => break };
                match rx_ref.try_recv() { Ok(m) => m, Err(_) => break }
            };
            match msg {
                ScanMsg::Progress(p) => self.progress = Some(p),
                ScanMsg::Finished(root) => { self.tree = Some(root); self.rx = None; }
            }
        }

        // 表示ノード選択
        let current = if self.bread.is_empty() {
            self.tree.as_deref()
        } else {
            unsafe { Some(&*self.bread[self.bread.len()-1]) }
        };

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(node) = current {
                if !self.bread.is_empty() && ui.button("<- 戻る").clicked() {
                    self.bread.pop();
                }
                ui.heading(format!("{} ({} items)", node.name, node.children.len()));

                // 固定ヘッダー
                ui.horizontal(|ui| {
                    let total_width = ui.available_width();
                    let label_w = total_width * 0.7;
                    let bar_w = total_width - label_w;
                    ui.allocate_ui(egui::Vec2::new(label_w, 20.0), |ui| {
                        ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                            ui.label("Name");
                        });
                    });
                    ui.allocate_ui(egui::Vec2::new(bar_w, 20.0), |ui| {
                        ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                            ui.label("Usage");
                        });
                    });
                });
                // スクロール可能な行
                ScrollArea::vertical().show(ui, |ui| {
                    let total_width = ui.available_width();
                    let label_w = total_width * 0.7;
                    let bar_w = total_width - label_w;
                    for child in &node.children {
                        let pct = child.size as f64 / node.size as f64 * 100.0;
                        ui.horizontal(|ui| {
                            // Name cell
                            let resp = ui.add_sized(
                                [label_w, 20.0],
                                egui::SelectableLabel::new(false, &*child.name)
                            );
                            if resp.clicked() && !child.children.is_empty() {
                                self.bread.push(&**child as *const DirNode);
                            }
                            let popup_id = Id::new(format!("menu-{}", child.path.display()));
                            if resp.secondary_clicked() {
                                ui.memory_mut(|m: &mut Memory| m.toggle_popup(popup_id));
                            }
                            egui::popup::popup_above_or_below_widget(
                                ui,
                                popup_id,
                                &resp,
                                egui::AboveOrBelow::Below,
                                PopupCloseBehavior::CloseOnClickOutside,
                                |ui| {
                                    ui.set_min_width(150.0);
                                    if ui.button("パスのコピー").clicked() {
                                        ui.output_mut(|o| o.copied_text = child.path.display().to_string());
                                        ui.memory_mut(|m: &mut Memory| m.close_popup());
                                    }
                                    if ui.button("Finderで表示").clicked() {
                                        if child.path.is_file() {
                                            let _ = Command::new("open")
                                                .arg("-R")
                                                .arg(&child.path)
                                                .spawn();
                                        } else {
                                            let _ = open::that(&child.path);
                                        }
                                    }
                                },
                            );
                            // Usage cell
                            ui.add_sized([bar_w, 20.0], egui::ProgressBar::new(pct as f32 / 100.0)
                                .desired_width(bar_w)
                                .text(format!("{:.1}%", pct))
                            );
                        }); // end horizontal
                    }
                });
            } else {
                ui.label("No data…");
            }
        });

        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

// 日本語フォント設定関数は消さないでください
fn setup_jp_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    if let Ok(bytes) = std::fs::read("assets/NotoSansJP-VariableFont_wght.ttf") {
        fonts.font_data.insert(
            "noto".into(),
            FontData::from_owned(bytes).into(),
        );
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "noto".into());
    }
    ctx.set_fonts(fonts);
}

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
