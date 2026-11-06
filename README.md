# Fellor File

Файловый менеджер для macOS на Rust: iPhone/iPad, Android, камеры и любые тома ↔ Mac по USB, без iTunes.
Спецификация: [SPEC.md](SPEC.md).

## Сборка

```sh
curl https://sh.rustup.rs -sSf | sh     # если Rust ещё не установлен
cargo build --release
```

## Запуск

```sh
cargo run --release -p fellor-app            # GUI
cargo run --release -p fellor-app -- --demo  # GUI с демо-устройством
./scripts/bundle.sh                          # собрать Fellor.app
cargo run -p fellor-cli -- doctor            # диагностика
cargo run -p fellor-cli -- devices
cargo run -p fellor-cli -- ls ios:/Media/DCIM
cargo run -p fellor-cli -- cp ios:/Media/DCIM ~/Pictures/iPhone
cargo run -p fellor-cli -- import --device ios --dest ~/Pictures/Import
```

Проверка протоколов на реальном устройстве:

```sh
RUST_LOG=debug cargo run -p fellor-ios --example ios_probe   # iPhone: нажмите «Доверять»
RUST_LOG=debug cargo run -p fellor-mtp --example mtp_probe   # Android: режим «Передача файлов»
```

## Структура

| Крейт | Назначение |
|---|---|
| fellor-vfs | Общий контракт: `Backend`, `VPath`, ошибки |
| fellor-local | Локальная ФС и тома в /Volumes |
| fellor-usbmux, fellor-lockdown, fellor-afc, fellor-ios | Стек iOS: usbmuxd → lockdown (TLS) → AFC / house_arrest |
| fellor-ptp, fellor-mtp | Стек Android/камер: PTP/MTP поверх USB (nusb) |
| fellor-transfer | Очередь передач, конфликты, докачка |
| fellor-index | Индекс быстрого импорта (SQLite) |
| fellor-devices | Объединение обнаружения устройств |
| fellor-cli | CLI `fellor` |
| apps/fellor-app | GUI на egui |
