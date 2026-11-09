#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const API: &str = "https://wallhaven.cc/api/v1/search";
const THUMB: egui::Vec2 = egui::vec2(300.0, 200.0);
const PRESETS: [(&str, &str); 7] = [
    ("Paisagem escura", "dark landscape"),
    ("Névoa", "fog"),
    ("Natureza escura", "dark nature"),
    ("Montanhas", "mountains"),
    ("Minimalista", "minimalism"),
    ("Cidade à noite", "city night"),
    ("Espaço", "space"),
];
const RESOLUTIONS: [(&str, &str); 2] = [("2K+", "2560x1440"), ("4K+", "3840x2160")];
const SORTS: [(&str, &str); 4] = [
    ("Mais votados", "toplist"),
    ("Favoritos", "favorites"),
    ("Relevância", "relevance"),
    ("Aleatório", "random"),
];

#[derive(Deserialize, Clone)]
struct Thumbs {
    small: String,
}

#[derive(Deserialize, Clone)]
struct Wall {
    id: String,
    path: String,
    resolution: String,
    #[serde(default)]
    file_size: u64,
    thumbs: Thumbs,
}

#[derive(Deserialize)]
struct SearchResp {
    data: Vec<Wall>,
    #[serde(default)]
    meta: serde_json::Value,
}

enum Msg {
    Results { gen: u64, walls: Vec<Wall>, last_page: u64, total: u64 },
    Thumb { gen: u64, id: String, img: egui::ColorImage },
    Status(String),
    Failed(String),
}

struct App {
    ctx: egui::Context,
    agent: ureq::Agent,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    jobs: Sender<(u64, String, String)>,
    gen: Arc<AtomicU64>,
    query: String,
    res: usize,
    sort: usize,
    walls: Vec<Wall>,
    textures: HashMap<String, egui::TextureHandle>,
    page: u64,
    last_page: u64,
    total: u64,
    loading: bool,
    status: String,
    pending: Option<Wall>,
}

fn meta_num(meta: &serde_json::Value, key: &str) -> u64 {
    match meta.get(key) {
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn fetch(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, String> {
    let resp = agent.get(url).call().map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(100 * 1024 * 1024)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    Ok(buf)
}

fn save_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join("Pictures").join("Wallpapers")
}

#[cfg(windows)]
fn set_wallpaper(path: &std::path::Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "user32")]
    extern "system" {
        fn SystemParametersInfoW(action: u32, param: u32, pv: *mut core::ffi::c_void, win_ini: u32) -> i32;
    }
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SPI_SETDESKWALLPAPER, SPIF_UPDATEINIFILE | SPIF_SENDCHANGE
    unsafe { SystemParametersInfoW(0x0014, 0, wide.as_mut_ptr().cast(), 0x3) != 0 }
}

#[cfg(not(windows))]
fn set_wallpaper(_path: &std::path::Path) -> bool {
    false
}

impl App {
    fn new(ctx: egui::Context) -> Self {
        let tls = native_tls::TlsConnector::new().expect("TLS indisponível");
        let agent = ureq::AgentBuilder::new()
            .tls_connector(Arc::new(tls))
            .user_agent("wallpicker/0.1")
            .timeout(Duration::from_secs(90))
            .build();
        let (tx, rx) = channel();
        let (jobs, job_rx) = channel::<(u64, String, String)>();
        let gen = Arc::new(AtomicU64::new(0));

        // Pool fixo de threads para baixar e decodificar miniaturas.
        let job_rx = Arc::new(Mutex::new(job_rx));
        for _ in 0..6 {
            let (job_rx, agent, tx, gen, ctx) =
                (job_rx.clone(), agent.clone(), tx.clone(), gen.clone(), ctx.clone());
            std::thread::spawn(move || loop {
                let job = job_rx.lock().unwrap().recv();
                let Ok((g, id, url)) = job else { break };
                if g != gen.load(Ordering::Relaxed) {
                    continue; // busca antiga, descarta
                }
                let Ok(bytes) = fetch(&agent, &url) else { continue };
                let Ok(img) = image::load_from_memory(&bytes) else { continue };
                let rgba = img.to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                let img = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                let _ = tx.send(Msg::Thumb { gen: g, id, img });
                ctx.request_repaint();
            });
        }

        let mut app = Self {
            ctx,
            agent,
            tx,
            rx,
            jobs,
            gen,
            query: PRESETS[0].1.to_string(),
            res: 0,
            sort: 0,
            walls: Vec::new(),
            textures: HashMap::new(),
            page: 1,
            last_page: 1,
            total: 0,
            loading: false,
            status: String::new(),
            pending: None,
        };
        app.search(true);
        app
    }

    fn search(&mut self, reset: bool) {
        if reset {
            self.gen.fetch_add(1, Ordering::Relaxed);
            self.walls.clear();
            self.textures.clear();
            self.page = 1;
        } else {
            self.page += 1;
        }
        self.loading = true;
        self.status = "Buscando...".into();
        let sort = SORTS[self.sort].1;
        let mut url = format!(
            "{API}?q={}&categories=100&purity=100&atleast={}&ratios=16x9,16x10,21x9&sorting={sort}&order=desc&page={}",
            encode(self.query.trim()),
            RESOLUTIONS[self.res].1,
            self.page
        );
        if sort == "toplist" {
            url.push_str("&topRange=1y");
        }
        let (agent, tx, ctx) = (self.agent.clone(), self.tx.clone(), self.ctx.clone());
        let gen = self.gen.load(Ordering::Relaxed);
        std::thread::spawn(move || {
            let msg = match agent.get(&url).call() {
                Ok(r) => match r.into_json::<SearchResp>() {
                    Ok(r) => Msg::Results {
                        gen,
                        last_page: meta_num(&r.meta, "last_page"),
                        total: meta_num(&r.meta, "total"),
                        walls: r.data,
                    },
                    Err(e) => Msg::Failed(format!("Resposta inválida: {e}")),
                },
                Err(e) => Msg::Failed(format!("Falha na busca: {e}")),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    fn download(&mut self, wall: &Wall, apply: bool) {
        let (agent, tx, ctx, wall) = (self.agent.clone(), self.tx.clone(), self.ctx.clone(), wall.clone());
        self.status = format!("Baixando {} ({})...", wall.id, wall.resolution);
        std::thread::spawn(move || {
            let result = (|| -> Result<String, String> {
                let dir = save_dir();
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                let ext = wall.path.rsplit('.').next().unwrap_or("jpg");
                let file = dir.join(format!("wallhaven-{}.{ext}", wall.id));
                if !file.exists() {
                    let bytes = fetch(&agent, &wall.path)?;
                    let part = file.with_extension("part");
                    std::fs::write(&part, bytes).map_err(|e| e.to_string())?;
                    std::fs::rename(&part, &file).map_err(|e| e.to_string())?;
                }
                if apply {
                    if !set_wallpaper(&file) {
                        return Err("o Windows recusou a troca do papel de parede".into());
                    }
                    Ok(format!("Papel de parede aplicado: {}", file.display()))
                } else {
                    Ok(format!("Salvo em {}", file.display()))
                }
            })();
            let _ = tx.send(match result {
                Ok(s) => Msg::Status(s),
                Err(e) => Msg::Failed(format!("Erro no download: {e}")),
            });
            ctx.request_repaint();
        });
    }

    fn drain(&mut self) {
        let current = self.gen.load(Ordering::Relaxed);
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Results { gen, walls, last_page, total } if gen == current => {
                    self.loading = false;
                    self.last_page = last_page.max(1);
                    self.total = total;
                    for w in &walls {
                        let _ = self.jobs.send((gen, w.id.clone(), w.thumbs.small.clone()));
                    }
                    self.walls.extend(walls);
                    self.status = format!("{} imagens encontradas", self.total);
                }
                Msg::Thumb { gen, id, img } if gen == current => {
                    let tex = self.ctx.load_texture(&id, img, egui::TextureOptions::LINEAR);
                    self.textures.insert(id, tex);
                }
                Msg::Status(s) => self.status = s,
                Msg::Failed(s) => {
                    self.loading = false;
                    self.status = s;
                }
                _ => {}
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain();
        let mut do_search = false;

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text("Buscar (em inglês)")
                        .desired_width(260.0),
                );
                if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    do_search = true;
                }
                egui::ComboBox::from_id_salt("res")
                    .selected_text(RESOLUTIONS[self.res].0)
                    .width(60.0)
                    .show_ui(ui, |ui| {
                        for (i, (label, _)) in RESOLUTIONS.iter().enumerate() {
                            do_search |= ui.selectable_value(&mut self.res, i, *label).changed();
                        }
                    });
                egui::ComboBox::from_id_salt("sort")
                    .selected_text(SORTS[self.sort].0)
                    .show_ui(ui, |ui| {
                        for (i, (label, _)) in SORTS.iter().enumerate() {
                            do_search |= ui.selectable_value(&mut self.sort, i, *label).changed();
                        }
                    });
                do_search |= ui.button("Buscar").clicked();
                if ui.button("Abrir pasta").clicked() {
                    let dir = save_dir();
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::process::Command::new("explorer").arg(dir).spawn();
                }
            });
            ui.horizontal(|ui| {
                for (label, q) in PRESETS {
                    if ui.selectable_label(self.query == q, label).clicked() {
                        self.query = q.to_string();
                        do_search = true;
                    }
                }
            });
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak("Clique: escolher  |  Botão direito: só baixar");
                });
            });
        });

        let mut action: Option<(Wall, bool)> = None;
        let mut more = false;
        let mut ask: Option<Wall> = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                    for wall in &self.walls {
                        let (rect, resp) = ui.allocate_exact_size(THUMB, egui::Sense::click());
                        if !ui.is_rect_visible(rect) {
                            continue;
                        }
                        let painter = ui.painter();
                        match self.textures.get(&wall.id) {
                            Some(tex) => {
                                let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                                painter.image(tex.id(), rect, uv, egui::Color32::WHITE);
                            }
                            None => {
                                painter.rect_filled(rect, 0.0, egui::Color32::from_gray(28));
                            }
                        }
                        if resp.hovered() {
                            let strip = egui::Rect::from_min_max(
                                egui::pos2(rect.left(), rect.bottom() - 22.0),
                                rect.max,
                            );
                            painter.rect_filled(strip, 0.0, egui::Color32::from_black_alpha(170));
                            painter.text(
                                strip.left_center() + egui::vec2(6.0, 0.0),
                                egui::Align2::LEFT_CENTER,
                                format!("{}  ·  {:.1} MB", wall.resolution, wall.file_size as f64 / 1_048_576.0),
                                egui::FontId::proportional(13.0),
                                egui::Color32::WHITE,
                            );
                            painter.rect_stroke(rect, 0.0, egui::Stroke::new(2.0_f32, egui::Color32::WHITE));
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if self.pending.is_some() {
                            // diálogo de confirmação aberto: ignora cliques na grade
                        } else if resp.clicked() {
                            ask = Some(wall.clone());
                        } else if resp.secondary_clicked() {
                            action = Some((wall.clone(), false));
                        }
                    }
                });
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    if self.loading {
                        ui.spinner();
                    } else if self.walls.is_empty() {
                        ui.weak("Nenhuma imagem. Tente outro termo.");
                    } else if self.page < self.last_page {
                        more = ui.button("Carregar mais").clicked();
                    }
                });
                ui.add_space(8.0);
            });
        });

        if ask.is_some() {
            self.pending = ask;
        }
        if let Some(wall) = self.pending.clone() {
            let mut close = false;
            egui::Window::new("Usar como papel de parede?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    if let Some(tex) = self.textures.get(&wall.id) {
                        ui.add(egui::Image::new(tex).fit_to_exact_size(THUMB));
                    }
                    ui.label(format!(
                        "{}  ·  {:.1} MB",
                        wall.resolution,
                        wall.file_size as f64 / 1_048_576.0
                    ));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Aplicar").clicked() {
                            action = Some((wall.clone(), true));
                            close = true;
                        }
                        if ui.button("Só baixar").clicked() {
                            action = Some((wall.clone(), false));
                            close = true;
                        }
                        if ui.button("Cancelar").clicked() {
                            close = true;
                        }
                    });
                });
            if close || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.pending = None;
            }
        }
        if let Some((wall, apply)) = action {
            self.download(&wall, apply);
        }
        if do_search {
            self.search(true);
        } else if more {
            self.search(false);
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("WallPicker")
            .with_inner_size([1264.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "WallPicker",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc.egui_ctx.clone())))),
    )
}
