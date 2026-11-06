# Fellor File — спецификация

Файловый менеджер для macOS на Rust. Подключил телефон по USB — сразу видишь его файлы
рядом с папками Мака и перекидываешь в обе стороны. Без iTunes, без Finder-синхронизации,
без Android File Transfer.

Версия документа: 0.1 (2026-11-07). Статус: черновик к реализации.

---

## 1. Цель и границы

### 1.1 Что делаем

- **iPhone / iPad → Mac и обратно** по USB-кабелю. Фото и видео из DCIM, папка Downloads,
  документы приложений с включённым File Sharing (VLC, Documents, Kindle и т.п.).
- **Android → Mac и обратно** по USB (режим «Передача файлов» / MTP). Вся память и SD-карта.
- **Любые смонтированные носители**: флешки, SD-карты, внешние SSD, сетевые тома — всё,
  что macOS показывает в `/Volumes`.
- **Фотоаппараты и прочие PTP-устройства** — бесплатно получаем через тот же MTP/PTP-стек.
- Двухпанельный интерфейс: слева источник, справа приём. Копирование, перемещение,
  удаление, создание папок, переименование. Очередь передач с прогрессом, паузой,
  отменой, докачкой.
- «Быстрый импорт»: одна кнопка — забрать с телефона все новые фото/видео в выбранную папку.
- Автоопределение: воткнул кабель — устройство появилось в сайдбаре, без перезапуска.

### 1.2 Чего не делаем (non-goals)

- Бэкапы iPhone, синхронизация музыки/контактов/приложений, установка IPA.
- Джейлбрейк-функции, доступ к корню файловой системы iOS.
- Доступ к системным папкам iOS (PhotoData, iTunes_Control и т.п. — Apple их не отдаёт
  без джейлбрейка, и мы не притворяемся, что можем).
- Беспроводные подключения (Wi-Fi/Bluetooth) — отдельный этап v2, архитектура не мешает.
- Windows/Linux — v2. Ядро кроссплатформенное, но первый релиз только macOS.

### 1.3 Принципы

1. **Весь код пишется сами, на Rust.** Протоколы usbmuxd, lockdown, AFC, PTP/MTP
   реализуются в собственных крейтах. Сторонние крейты — только для нижнего уровня
   (USB-транспорт, TLS, plist, async-рантайм, GUI-тулкит). Никаких C-библиотек
   libimobiledevice/libmtp в зависимостях — чтобы не тащить brew, dylib-ы и проблемы
   с нотаризацией.
2. **Протокол за трейтом.** GUI и движок передач не знают, откуда файл — с iPhone, Android
   или с флешки. Всё ходит через `Backend`.
3. **Никогда не портить данные.** Любая запись — через временный файл + rename, где
   протокол это позволяет. Конфликты имён решает пользователь или явная политика.
4. **Понятные ошибки.** Не «AFC error 7», а «Разблокируйте iPhone и нажмите „Доверять“».

---

## 2. Как это физически работает

### 2.1 iPhone / iPad (USB)

macOS уже содержит демон **usbmuxd** (Apple Mobile Device Service). Он слушает Unix-сокет
`/var/run/usbmuxd` и мультиплексирует TCP-соединения в устройство. Нам не нужны ни драйверы,
ни iTunes, ни права root — любой процесс может подключиться к сокету.

Цепочка:

```
наш процесс ──unix socket──▶ usbmuxd ──USB──▶ iPhone
   1. usbmuxd: ListDevices / Listen            (plist по TCP-подобному протоколу)
   2. usbmuxd: ReadPairRecord(udid)            (pair record уже создан macOS при «Доверять»)
   3. usbmuxd: Connect(device, port 62078)     → соединение с lockdownd
   4. lockdownd: QueryType, StartSession(HostID, SystemBUID)  → TLS с host-сертификатом
   5. lockdownd: StartService("com.apple.afc") → порт сервиса (+ EnableServiceSSL)
   6. usbmuxd: Connect(device, afc_port)       → AFC-сессия
   7. AFC: ReadDir / FileRefOpen / FileRefRead / FileRefWrite ...
```

Сервисы, которые используем:

| Сервис | Зачем |
|---|---|
| `com.apple.afc` | Медиа-песочница `/var/mobile/Media`: `DCIM/`, `Downloads/`, `Books/`, `PhotoData/` (только чтение части), и др. |
| `com.apple.mobile.house_arrest` | Папка `Documents/` приложений с `UIFileSharingEnabled` (команда `VendDocuments`). |
| `com.apple.mobile.installation_proxy` | Список приложений (`Browse`), чтобы показать, у каких есть File Sharing. |
| `com.apple.mobile.notification_proxy` | Опционально: уведомления «устройство заблокировано/разблокировано». |

Ограничения, о которых честно говорим пользователю:

- Нужно один раз нажать «Доверять этому компьютеру» на телефоне. Pair record macOS
  хранит в `/var/db/lockdown/<udid>.plist`; через usbmuxd мы его читаем без root.
- Телефон должен быть разблокирован при первом подключении сессии (lockdown возвращает
  `PasswordProtected`).
- Если включена «Оптимизация хранилища iCloud», оригиналы части фото физически не на
  телефоне — в DCIM их не будет. Это не наша проблема, но покажем подсказку.
- Live Photo — две сущности: `IMG_0001.HEIC` + `IMG_0001.MOV`. Режим импорта умеет
  забирать парой.
- iOS 17+ перевёл **developer**-сервисы на RSD/CoreDevice-туннель, но AFC, house_arrest
  и installation_proxy по-прежнему живут в старом lockdown через usbmuxd. Для наших
  задач туннель не нужен.

### 2.2 Android (USB, MTP)

macOS не умеет MTP нативно. Мы говорим с устройством напрямую по USB через
IOKit (крейт `nusb`, чистый Rust, без libusb).

- Пользователь переключает USB-режим на телефоне в «Передача файлов (MTP)». До этого
  интерфейс MTP либо отсутствует, либо отвечает нулём хранилищ. Показываем подсказку.
- Интерфейс: класс `0x06` / подкласс `0x01` / протокол `0x01` (Still Image), либо
  vendor-specific `0xFF` с interface string `"MTP"`. Три эндпоинта: bulk IN, bulk OUT,
  interrupt IN (события).
- **Конфликт с Android File Transfer / OpenMTP / Google's Android File Transfer Agent**:
  они захватывают интерфейс эксклюзивно. При ошибке `exclusive access` детектим по
  списку процессов и предлагаем закрыть.
- Протокол PTP (ISO 15740) + расширения MTP. Все операции — синхронные транзакции
  «команда → [данные] → ответ» в одной сессии. Параллелизма в одной сессии нет, поэтому
  на одно Android-устройство один воркер.

Используемые операции (op-коды):

| Код | Операция | Назначение |
|---|---|---|
| 0x1001 | GetDeviceInfo | Модель, поддерживаемые опкоды/свойства |
| 0x1002 / 0x1003 | OpenSession / CloseSession | Сессия |
| 0x1004 / 0x1005 | GetStorageIDs / GetStorageInfo | Внутренняя память, SD-карта |
| 0x1007 | GetObjectHandles | Список объектов в папке (parent handle) |
| 0x1008 | GetObjectInfo | Метаданные одного объекта (медленно, fallback) |
| 0x9805 | GetObjectPropList | Пакетные метаданные всей папки за один вызов (быстрый листинг) |
| 0x1009 | GetObject | Скачать файл целиком |
| 0x101B | GetPartialObject | Докачка (32-битный offset) |
| 0x95C4 | GetPartialObject64 | Докачка >4 ГБ (Android-расширение) |
| 0x100C / 0x100D | SendObjectInfo / SendObject | Загрузить файл (создание папки — объект с форматом 0x3001 Association) |
| 0x9808 | SendObjectPropList | Загрузка с 64-битным размером |
| 0x100B | DeleteObject | Удаление |
| 0x9803 / 0x9804 | GetObjectPropValue / SetObjectPropValue | Переименование (свойство 0xDC07 ObjectFileName), даты |
| 0x1019 | MoveObject | Перемещение внутри устройства |

Контейнер PTP: `length:u32 | type:u16 (1 cmd, 2 data, 3 resp, 4 event) | code:u16 |
transaction_id:u32 | params: до 5 × u32`. Все little-endian. Строки — UCS-2 с префиксом
длины в символах и нулевым терминатором.

MTP не знает путей — только handle-ы и parent handle. VFS-слой строит дерево и
кэширует его на время сессии; инвалидация по событиям `ObjectAdded/ObjectRemoved`
(0x4002/0x4003) с interrupt-эндпоинта и после своих же операций.

### 2.3 Смонтированные тома

Обычная файловая система. Список томов — чтение `/Volumes` + подписка на
DiskArbitration (`DADiskAppeared/Disappeared`) через `objc2`/`core-foundation`; в v0 допустим
опрос раз в 2 секунды. Безопасное извлечение — `DADiskUnmount` / `DADiskEject`.

### 2.4 Обнаружение подключений

| Источник | Механизм |
|---|---|
| iOS | usbmuxd `Listen` — пушит `Attached` / `Detached` с `DeviceID`, `SerialNumber` (UDID), `ConnectionType` (USB/Network) |
| Android/PTP | `nusb::watch_devices()` — hotplug-события IOKit; фильтр по интерфейсу MTP/PTP |
| Тома | DiskArbitration или опрос `/Volumes` |

Все три источника сливаются в один поток `DeviceEvent` для GUI.

---

## 3. Архитектура

### 3.1 Workspace

```
fellor_file/
├── Cargo.toml                 # workspace
├── crates/
│   ├── fellor-vfs/            # трейт Backend, типы Entry/Path/Progress, ошибки
│   ├── fellor-local/          # Backend для локальной ФС и томов
│   ├── fellor-usbmux/         # клиент /var/run/usbmuxd (plist-протокол)
│   ├── fellor-lockdown/       # lockdownd: pairing records, TLS-сессия, StartService
│   ├── fellor-afc/            # AFC + house_arrest + installation_proxy
│   ├── fellor-ios/            # Backend для iOS поверх afc/house_arrest + discovery
│   ├── fellor-ptp/            # PTP/MTP: контейнеры, кодеки, операции, транспорт nusb
│   ├── fellor-mtp/            # Backend для Android/камер поверх ptp + discovery
│   ├── fellor-transfer/       # движок передач: очередь, воркеры, конфликты, докачка
│   ├── fellor-index/          # SQLite-индекс «что уже импортировано» для быстрого импорта
│   └── fellor-cli/            # CLI для отладки и скриптов: fellor ls ios:/DCIM
└── apps/
    └── fellor-app/            # GUI (eframe/egui), macOS bundle, меню-бар агент
```

Слои снизу вверх: транспорт → протокол → Backend → transfer engine → UI.
Верхний слой не импортирует ничего ниже Backend.

### 3.2 Трейт `Backend` (fellor-vfs)

```rust
#[async_trait]
pub trait Backend: Send + Sync {
    fn id(&self) -> BackendId;                       // стабильный ключ: "ios:<udid>", "mtp:<serial>", "local"
    fn info(&self) -> DeviceInfo;                    // имя, модель, иконка, capabilities
    fn capabilities(&self) -> Capabilities;          // read/write/delete/rename/move/mkdir/set_mtime/partial_read/partial_write/free_space

    async fn roots(&self) -> Result<Vec<Entry>>;     // iOS: Media + по папке на каждое app с File Sharing; MTP: хранилища
    async fn list(&self, path: &VPath) -> Result<Vec<Entry>>;
    async fn stat(&self, path: &VPath) -> Result<Entry>;
    async fn free_space(&self, root: &VPath) -> Result<Option<SpaceInfo>>;

    async fn open_read(&self, path: &VPath, offset: u64) -> Result<Box<dyn AsyncRead + Send + Unpin>>;
    async fn create_write(&self, path: &VPath, size_hint: Option<u64>) -> Result<Box<dyn WriteSink>>;
        // WriteSink: AsyncWrite + finish() -> Result<Entry>  (commit временного файла / SendObject)

    async fn mkdir(&self, path: &VPath) -> Result<Entry>;
    async fn remove(&self, path: &VPath, recursive: bool) -> Result<()>;
    async fn rename(&self, from: &VPath, to: &VPath) -> Result<Entry>;   // в пределах одного backend
    async fn set_mtime(&self, path: &VPath, mtime: SystemTime) -> Result<()>;

    fn watch(&self) -> Option<BroadcastStream<FsEvent>>;  // изменения на устройстве, если протокол умеет
}

pub struct Entry {
    pub name: String,
    pub path: VPath,
    pub kind: EntryKind,            // File | Dir | Symlink | Special
    pub size: Option<u64>,
    pub mtime: Option<SystemTime>,
    pub ctime: Option<SystemTime>,
    pub mime: Option<&'static str>, // по расширению / PTP format code
    pub hidden: bool,
    pub native: NativeId,           // MTP handle / AFC path / local PathBuf — для backend-а
}
```

`VPath` — абстрактный путь `BackendId + Vec<Component>`, сериализуется как
`ios:ABCD1234/DCIM/100APPLE/IMG_0001.HEIC`, `mtp:R58M.../Internal shared storage/DCIM`,
`local:/Users/admin/Pictures`.

### 3.3 Движок передач (fellor-transfer)

```
TransferRequest { items: Vec<VPath>, dest: VPath, op: Copy|Move, policy: ConflictPolicy }
      │
      ▼
   Planner  — рекурсивно обходит источник, строит список Job { src, dst, size },
              считает общий объём, заранее проверяет свободное место и конфликты
      │
      ▼
   Scheduler — воркеры по backend-у: local ↔ local до 4 параллельных,
               любой backend устройства — 1 (AFC и MTP последовательны в сессии);
               копия устройство → устройство идёт через локальный temp-файл
      │
      ▼
   Copier  — чанки 1 МиБ (AFC) / 4 МиБ (MTP) / 8 МиБ (local), прогресс каждые ≥100 мс,
             докачка через open_read(offset) если WriteSink упал на полпути,
             set_mtime после finish(), опциональная верификация размера
      │
      ▼
   Events: Queued, Started, Progress{done, total, bps, eta}, Conflict{..}, Failed{err, retryable}, Done
```

- **Политика конфликтов**: `Ask` (по умолчанию, с «применить ко всем»), `Skip`,
  `Overwrite`, `Rename` (`IMG_0001 (2).HEIC`), `KeepNewer`, `SkipIfSameSize`.
- **Move** = Copy + Remove источника только после успешного `finish()`; внутри одного
  backend — `rename`/`MoveObject`.
- **Пауза/отмена**: кооперативная через `CancellationToken`; при отмене незавершённый
  файл приёмника удаляется.
- **Ретраи**: до 3 для ошибок транспорта (USB stall, usbmuxd reset) с переподключением
  сессии; не ретраим логические (нет места, нет прав).
- Очередь переживает отключение устройства: задания переходят в `Waiting for device`
  и возобновляются по `Attached` с тем же id.

### 3.4 Быстрый импорт (fellor-index)

SQLite (`rusqlite`, bundled) в `~/Library/Application Support/Fellor/index.db`:
`imported(device_id, rel_path, size, mtime, sha_prefix, imported_at, dest)`.
«Новое» = нет записи с таким `(device_id, rel_path, size, mtime)`. Папка назначения
по умолчанию `~/Pictures/Fellor Import/<YYYY-MM-DD>/`, шаблон настраивается. После
успешного импорта — опционально удалить с устройства (с явным подтверждением).

Конвертация HEIC→JPEG — через системный `sips` (есть в каждой macOS), без своих
HEIF-декодеров. Опция, по умолчанию выключена: оригиналы ценнее.

---

## 4. Протокольные крейты — детали реализации

### 4.1 fellor-usbmux

- Подключение: `tokio::net::UnixStream` к `/var/run/usbmuxd`.
- Пакет: `length:u32 | version:u32 (=1) | type:u32 (=8 plist) | tag:u32 | payload: XML plist`.
- Сообщения: `ListDevices`, `Listen`, `Connect {DeviceID, PortNumber (big-endian!)}`,
  `ReadPairRecord {PairRecordID}`, `ReadBUID`. Ответ `Result` с `Number` (0 = ок,
  2 = device not found, 3 = port not available, 5 = malformed).
- После успешного `Connect` сокет превращается в сырой TCP-канал к устройству —
  возвращаем его как `UsbmuxStream: AsyncRead + AsyncWrite`.
- `Listen` держит отдельное соединение, отдаёт `Stream<DeviceEvent>`.

### 4.2 fellor-lockdown

- Пакет lockdown: `length:u32 (big-endian) | XML plist`.
- Последовательность: `QueryType` → `GetValue` (DeviceName, ProductType, ProductVersion,
  UniqueDeviceID) → `StartSession {HostID, SystemBUID}` из pair record →
  если `EnableSessionSSL` — поднять TLS **поверх того же сокета**: клиентский сертификат
  `HostCertificate` + `HostPrivateKey` из pair record, серверный `DeviceCertificate`
  самоподписанный → принимаем через кастомный verifier, сверяя с pair record.
  TLS: `rustls` (iOS ≥ 10 говорит TLS 1.2, этого достаточно; для старых устройств
  честно говорим «не поддерживается»).
- `StartService {Service}` → `{Port, EnableServiceSSL}`. Для AFC SSL обычно выключен,
  для house_arrest — включён на новых iOS; обрабатываем оба случая.
- Ошибки: `PasswordProtected` → «Разблокируйте iPhone»; `InvalidHostID` /
  `InvalidPairRecord` → «Переподключите кабель и нажмите „Доверять“»;
  `UserDeniedPairing` → понятная подсказка.
- Pairing сами не делаем в v1 — macOS делает это при первом подключении (диалог
  «Доверять» появляется системно). Если pair record нет — показываем инструкцию.
  Собственный pairing (генерация host cert, `Pair`, escrow bag) — v2.

### 4.3 fellor-afc

- Пакет: заголовок 40 байт: `magic "CFA6LPAA" | entire_length:u64 | this_length:u64 |
  packet_num:u64 | operation:u64`, затем payload. Строки нуль-терминированные,
  список — через `\0`.
- Операции: `ReadDir (3)`, `GetFileInfo (10)`, `MakeDir (9)`, `RemovePath (8)`,
  `RenamePath (24)`, `FileRefOpen (13)`, `FileRefRead (15)`, `FileRefWrite (16)`,
  `FileRefSeek (17)`, `FileRefClose (20)`, `SetFileTime (30)`, `GetDeviceInfo (11)`
  (FSTotalBytes/FSFreeBytes), `RemovePathAndContents (34)`.
- Чтение: `FileRefRead` блоками 1 МиБ. Запись: `FileRefWrite` с данными в том же пакете.
- Запись атомарно: пишем в `<dir>/.fellor-tmp-<uuid>`, затем `RenamePath`.
- house_arrest: `StartService("com.apple.mobile.house_arrest")` → plist
  `{Command: VendDocuments, Identifier: <bundle_id>}` → `{Status: Complete}` → далее
  на этом же сокете говорим чистый AFC с корнем в песочнице приложения, рабочая папка
  `/Documents`.
- installation_proxy: `{Command: Browse, ClientOptions: {ApplicationType: User,
  ReturnAttributes: [CFBundleIdentifier, CFBundleDisplayName, UIFileSharingEnabled]}}`,
  ответ приходит чанками `Status: BrowsingApplications` → `Complete`.

### 4.4 fellor-ptp

- Транспорт: `nusb` — `Device::open` → `claim_interface` → bulk transfers с таймаутом
  (5 с команда, 30 с данные). Конкурентность: `Mutex<Session>` — одна транзакция
  за раз, `transaction_id` монотонный.
- `Container` encode/decode с полным покрытием unit-тестами на «сырых» байтах,
  снятых с реальных Pixel/Samsung (фикстуры в `tests/fixtures/*.bin`).
- Data-фаза произвольного размера — стримим в `AsyncRead` без буферизации всего файла
  (MTP отдаёт длину в заголовке первого пакета; для файлов > 4 ГиБ Android шлёт
  `0xFFFFFFFF` и мы читаем до короткого пакета / по известному размеру из
  `ObjectCompressedSize64`).
- Zero-length packet: если размер данных кратен `wMaxPacketSize`, устройство шлёт ZLP —
  обрабатываем на обоих направлениях.
- Быстрый листинг: `GetObjectPropList(handle=0xFFFFFFFF?, format=0, prop=0xFFFFFFFF
  (all), parent)` — одна транзакция на папку. Если устройство не поддерживает 0x9805
  — fallback на `GetObjectHandles` + `GetObjectInfo` по одному.
- Загрузка: `SendObjectInfo(storage, parent)` + `SendObject` (данные). Если размер
  > 4 ГиБ или устройство поддерживает 0x9808 — `SendObjectPropList` с
  `ObjectCompressedSize64 (0xDC04? → используем 0xDC04 ObjectSize как u64 в MTP)`.
- Устройство может отвечать `Device_Busy (0x2019)` — ждём 200 мс и повторяем до 10 раз
  (типично сразу после разблокировки экрана).
- Reset-процедура при зависании: `Cancel request` (class-specific control 0x64) →
  `Device Reset (0x66)` → переоткрыть сессию.

---

## 5. GUI (apps/fellor-app)

### 5.1 Стек

`eframe`/`egui` — чистый Rust, быстро собирается, достаточен для двухпанельника.
Нативные вещи через `objc2`/`objc2-app-kit`: диалоги Open/Save (`rfd`), уведомления
(`UNUserNotificationCenter`), Dock badge с прогрессом, меню-бар иконка
(`NSStatusItem`), «Показать в Finder». Drag из приложения в Finder (NSDraggingSession)
— v1.1, в v1 — кнопка/шорткат «Копировать в…».

Альтернативы, если egui упрётся в нативность: `gpui` (Zed), `Tauri 2` + лёгкий
фронт. Решение на M0 после прототипа.

### 5.2 Разметка

```
┌──────────────────────────────────────────────────────────────────────┐
│ ▣ Fellor File   [⟲]   [Быстрый импорт ▾]                   [⚙] [?]   │
├──────────┬────────────────────────────┬──────────────────────────────┤
│ Устройст.│ ◀ ▶ ios:iPhone › DCIM › 100APPLE  │ ◀ ▶ ~/Pictures › 2026   │
│ ● iPhone │ имя         размер   дата  │ имя          размер   дата   │
│ ● Pixel 8│ IMG_0412.HEIC 3.1 MB 11.06 │ ...                          │
│ ○ SD 64G │ IMG_0412.MOV  5.8 MB 11.06 │                              │
│ Мак      │ IMG_0413.HEIC 2.9 MB 11.06 │                              │
│  Pictures│                            │                              │
│  Downl.  │                            │                              │
│  Desktop │ 142 файла · 1.3 ГБ · 9 выбр.│ 38 файлов · 520 ГБ свободно  │
├──────────┴────────────────────────────┴──────────────────────────────┤
│ Передачи (2 активных, 14 в очереди)      1.2 ГБ / 4.7 ГБ  38 МБ/с ⏸ ✕ │
│ ▸ IMG_0412.HEIC → ~/Pictures/2026           ████████░░ 80%           │
│ ▸ DCIM/100APPLE (118 файлов)                ██░░░░░░░░ 21%  ~1:40    │
└──────────────────────────────────────────────────────────────────────┘
```

- Сайдбар: устройства (с индикатором состояния: подключено / нужно разблокировать /
  нужен режим MTP / занято другой программой), тома, избранные папки Мака.
- Панели: таблица с сортировкой, фильтр по имени (⌘F), превью изображений по пробелу
  (локальные + загрузка миниатюры по требованию: MTP `GetThumb 0x100A`, AFC — читаем
  первые байты и декодируем `image`-крейтом, HEIC — через `sips` в кэш).
- Нижняя панель: очередь передач, сворачивается.
- Контекстное меню: Копировать в другую панель, Переместить, Переименовать, Новая
  папка, Удалить, Показать в Finder (только local), Свойства.

### 5.3 Горячие клавиши

| Клавиша | Действие |
|---|---|
| Tab | Переключить активную панель |
| F5 / ⌘C→⌘V между панелями | Копировать в противоположную панель |
| F6 | Переместить |
| F7 / ⌘⇧N | Новая папка |
| F8 / ⌘⌫ | Удалить (с подтверждением; на устройстве — без корзины, предупреждаем) |
| Enter | Открыть папку / открыть файл в системе (local) |
| ⌘↑ / ⌫ | Вверх |
| Space | Быстрый просмотр |
| ⌘I | Быстрый импорт с активного устройства |
| ⌘E | Извлечь том |
| ⌘, | Настройки |

### 5.4 Состояния устройства в UI

| Состояние | Что показываем |
|---|---|
| iOS, нет pair record | «Нажмите „Доверять“ на iPhone и введите код-пароль» + кнопка «Проверить снова» |
| iOS, `PasswordProtected` | «Разблокируйте iPhone» |
| Android, нет MTP-интерфейса | «На телефоне: Уведомление „Зарядка через USB“ → „Передача файлов“» |
| Android, интерфейс занят | «Закройте Android File Transfer / OpenMTP» + кнопка «Закрыть за меня» (`kill` по имени, с подтверждением) |
| Android, 0 хранилищ | «Разблокируйте экран телефона» |
| Устройство отвалилось во время передачи | Задания в «Ожидание устройства», тост |

### 5.5 Меню-бар агент

Опция «Запускать при входе» (`SMAppService`). В фоне слушает usbmuxd/nusb; при
подключении телефона показывает уведомление «iPhone подключён — импортировать
N новых фото?» с кнопками «Импортировать» / «Открыть Fellor». Так достигается
«воткнул — скинул» за два клика.

---

## 6. CLI (fellor-cli)

Нужен для отладки протоколов без GUI и для скриптов:

```
fellor devices                          # список устройств и состояний
fellor ls  ios:/DCIM/100APPLE           # листинг
fellor cp  ios:/DCIM/100APPLE ~/Photos  # копирование с прогрессом
fellor cp  ~/movie.mp4 mtp:/Internal\ shared\ storage/Movies/
fellor rm  mtp:/sdcard/tmp.bin
fellor import --device ios --dest ~/Pictures/Import --delete-after
fellor doctor                           # диагностика: usbmuxd доступен? pair record? кто держит USB?
```

---

## 7. Безопасность, упаковка, macOS-специфика

- Приложение **не** в App Sandbox (иначе нужен `com.apple.security.device.usb` и
  доступ к usbmuxd-сокету становится проблемным). Hardened Runtime — да.
- Подпись Developer ID + нотаризация (`notarytool`), сборка бандла через
  `cargo-packager` или `cargo-bundle`; `Info.plist` с `LSUIElement` для агента.
- Запрашиваемые разрешения: Уведомления. Full Disk Access не нужен. Для записи в
  `~/Pictures`, `~/Downloads`, `~/Desktop` система сама спросит TCC-доступ при первом
  обращении — обработать отказ (`EPERM`) понятным сообщением.
- Никакой телеметрии. Логи — `tracing` в `~/Library/Logs/Fellor/`, с ротацией,
  без содержимого файлов и без UDID в полном виде (последние 4 символа).
- Pair record и TLS-ключи в памяти не кэшируем дольше сессии; на диск не пишем.
- Универсальный бинарь arm64 + x86_64 (`lipo`). Минимальная macOS 13.

---

## 8. Производительность — целевые цифры

| Сценарий | Цель |
|---|---|
| Листинг папки DCIM с 2000 файлов (iOS AFC) | < 1.5 с |
| Листинг папки с 2000 файлов (Android, GetObjectPropList) | < 2 с |
| Чтение с iPhone по USB 2.0 | ≥ 30 МБ/с (упираемся в USB 2.0 телефона) |
| Чтение с Android по USB 3 | ≥ 100 МБ/с |
| Запуск приложения до интерактивного окна | < 400 мс |
| Память при очереди в 10 000 файлов | < 300 МБ |

Чанкование и отсутствие буферизации целого файла — обязательны, большие видео
(10+ ГБ) должны проходить с постоянной памятью.

---

## 9. Тестирование

- **Unit**: кодеки пакетов usbmux/lockdown/AFC/PTP на байтовых фикстурах; VPath;
  планировщик конфликтов; политика ретраев. Запускаются в CI без устройств.
- **Protocol replay**: записанные диалоги с реальными устройствами (iPhone 15 /
  iOS 18, Pixel 8 / Android 15, Samsung S24) проигрываются через fake-транспорт.
- **Integration (feature `hw-tests`)**: реальные устройства на машине разработчика;
  скрипт `scripts/hw-smoke.sh` — ls, cp 1 файл, cp 500 файлов, cp 5 ГБ, rm, mkdir,
  отключение кабеля посреди копирования.
- **GUI**: smoke через `egui_kittest` на ключевые экраны.
- **Матрица устройств для релиза**: iPhone (iOS 16, 17, 18, 26), iPad, Pixel, Samsung
  (у Samsung свой MTP-стек с особенностями), Xiaomi (HyperOS), камера Sony/Canon по PTP,
  флешка exFAT, SD-карта через встроенный ридер.

---

## 10. План работ (milestones)

| # | Этап | Результат | Оценка |
|---|---|---|---|
| M0 | Скаффолд | Workspace, `fellor-vfs`, `fellor-local`, CLI `ls/cp` для локальных путей, движок передач с прогрессом, пустое окно egui с двумя панелями по локальной ФС | 1 нед |
| M1 | iOS чтение | usbmux + lockdown + AFC: список устройств, листинг, скачивание из DCIM, в GUI появляется iPhone | 2 нед |
| M2 | iOS запись + приложения | Запись/удаление/mkdir/rename, house_arrest + installation_proxy, Live Photo парами | 1 нед |
| M3 | Android чтение | nusb-транспорт, PTP-контейнеры, GetObjectPropList-листинг, GetObject/GetPartialObject, hotplug | 2 нед |
| M4 | Android запись | SendObjectInfo/SendObject, >4 ГБ, Delete/Move/Rename, события ObjectAdded | 1 нед |
| M5 | Движок и UX | Конфликты, пауза/отмена, докачка, «ожидание устройства», превью, шорткаты, состояния устройств в UI | 2 нед |
| M6 | Быстрый импорт + агент | SQLite-индекс, пресет импорта, меню-бар, уведомления, автозапуск | 1 нед |
| M7 | Релиз | Подпись, нотаризация, universal binary, DMG, матрица устройств, `fellor doctor` | 1 нед |

Итого ~11 недель до 1.0 одним разработчиком. M1 и M3 независимы — можно
параллелить.

---

## 11. Зависимости (внешние крейты)

Только инфраструктурные. Весь протокольный код — свой.

| Крейт | Роль |
|---|---|
| `tokio` | async-рантайм, UnixStream |
| `nusb` | USB через IOKit без libusb |
| `rustls` + `rustls-pemfile` / `rcgen` | TLS для lockdown; rcgen — на будущее для своего pairing |
| `plist` | XML/binary plist для usbmux/lockdown/house_arrest |
| `eframe`, `egui`, `egui_extras` | GUI |
| `objc2`, `objc2-app-kit`, `objc2-foundation` | нативные вызовы macOS |
| `rfd` | системные диалоги файлов |
| `rusqlite` (bundled) | индекс импорта |
| `image` | превью JPEG/PNG |
| `thiserror`, `anyhow` | ошибки |
| `tracing`, `tracing-appender` | логи |
| `serde`, `serde_json`, `toml` | конфиг |
| `clap` | CLI |
| `tokio-util` | CancellationToken |
| `bytes` | буферы протоколов |

Запрещено: `libimobiledevice`, `libmtp`, `libusb`-биндинги, любой C-код в дереве
зависимостей сверх `rusqlite`-bundled.

---

## 12. Открытые вопросы

1. egui vs gpui для финального GUI — решить по прототипу в M0.
2. Нужен ли собственный pairing (без первого «Доверять» через macOS)? Пока нет —
   macOS делает это сам при любом подключении.
3. Конвертация HEIC — через `sips` или оставить только копирование оригиналов?
4. Поддержка iOS < 13 — не целимся; подтвердить.
5. Беспроводной режим (lockdown по Wi-Fi уже поддерживается usbmuxd как
   `ConnectionType: Network`) — оставить в v2 или дать как скрытую опцию раньше.
