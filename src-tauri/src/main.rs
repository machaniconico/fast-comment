// Windows リリースビルドでコンソール窓を出さない。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! デスクトップ専用エントリポイント。実体は `fast_comment_lib::run()`。

fn main() {
    fast_comment_lib::run();
}
