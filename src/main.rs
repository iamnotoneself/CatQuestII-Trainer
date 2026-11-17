#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// 扫描/反查/聚类等完整移植的 API 目前只有部分被 GUI 直接调用，
// 但它们是修改器工具链的一部分（也用于自检与后续扩展），故保留。
#![allow(dead_code)]

mod actions;
mod gui;
mod keepers;
mod locate;
mod memory;
mod mono;
mod scan;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([920.0, 720.0])
            .with_min_inner_size([820.0, 620.0])
            .with_title(gui::APP_TITLE),
        ..Default::default()
    };
    eframe::run_native(
        gui::APP_TITLE,
        options,
        Box::new(|cc| Ok(Box::new(gui::TrainerApp::new(cc)))),
    )
}
