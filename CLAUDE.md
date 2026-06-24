# CLAUDE.md — remotedesk

## Qué es este proyecto

App de escritorio remoto LAN-only escrita en Rust. Un solo binario que corre en macOS (servidor) y Windows (cliente). El servidor captura la pantalla del Mac a 120fps en resolución nativa (detectada automáticamente del display), la codifica con VideoToolbox (H.264 hardware), y la transmite via TCP a un cliente Windows que decodifica y renderiza el stream. Los inputs de mouse y teclado viajan en sentido inverso.

No hay relay, no hay intermediarios, no hay cloud. Conexión directa IP:puerto en LAN.

---

## Reglas de trabajo del agente

### Preguntar antes de asumir

Si algo es ambiguo (una API, un comportamiento esperado, una decisión de diseño), pregunta antes de escribir código. No asumas la interpretación más conveniente.

### Solución más simple primero

Implementa lo mínimo que funcione. No agregues abstracciones, generics, traits complejos ni flexibilidad que no fue pedida explícitamente. Si la solución simple funciona, es la correcta.

### No tocar código no relacionado

Si un archivo o función no es parte directa de la tarea actual, no lo modifiques aunque creas que podría mejorarse.

### Marcar incertidumbre explícitamente

Si no estás seguro de un enfoque o detalle técnico, dilo antes de proceder con un comentario `// INCERTIDUMBRE:` en el código y una nota en el output. No finjas confianza.

### Loop de autocorrección del agente

Después de cada implementación de un módulo:
1. Ejecuta `cargo build` y corrige todos los errores de compilación antes de continuar.
2. Ejecuta los tests del módulo con `cargo test <modulo>`.
3. Si algún test falla, analiza el error, corrige el código, y repite desde el paso 1.
4. No avances al siguiente módulo hasta que el actual compile y sus tests pasen.
5. Máximo 3 intentos de corrección por fallo. Si al tercer intento sigue fallando, detente y reporta el problema con detalle.

---

## Stack

- **Rust edition 2021**
- **eframe 0.27** — UI (egui + winit + wgpu incluidos)
- **tokio 1 (full)** — async runtime
- **local-ip-address 0.6** — obtener IP local
- **VideoToolbox** (FFI, macOS) — encoding H.264 hardware
- **ScreenCaptureKit** (FFI, macOS) — captura de pantalla
- **CoreGraphics** (FFI, macOS) — inyección de inputs
- **ffmpeg-next** — decoding H.264 en Windows
- **windows crate 0.56** — inputs en Windows
- **tracing + tracing-subscriber** — logging estructurado
- **anyhow** — manejo de errores con contexto

---

## Estructura del proyecto

```
remotedesk/
├── CLAUDE.md
├── Cargo.toml
├── build.rs                  # links a frameworks macOS si target = macos
├── src/
│   ├── main.rs               # entry point, tokio runtime, lanza egui
│   ├── ui.rs                 # eframe::App, estados de UI
│   ├── network.rs            # TCP server y client, compartido
│   ├── protocol.rs           # structs de mensajes serializados
│   └── platform/
│       ├── mod.rs            # re-exports condicionales
│       ├── macos.rs          # captura + encoding + inject inputs
│       └── windows.rs        # captura de inputs, envío al servidor
└── tests/
    ├── network_tests.rs
    ├── protocol_tests.rs
    └── platform_tests.rs
```

---

## Cargo.toml

```toml
[package]
name = "remotedesk"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "remotedesk"
path = "src/main.rs"

[dependencies]
eframe = "0.27"
tokio = { version = "1", features = ["full"] }
local-ip-address = "0.6"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
serde = { version = "1", features = ["derive"] }
bincode = "1"

[target.'cfg(target_os = "macos")'.dependencies]
core-foundation = "0.9"
core-graphics = "0.23"

[target.'cfg(target_os = "windows")'.dependencies]
windows = { version = "0.56", features = [
    "Win32_UI_Input_KeyboardAndMouse",
    "Win32_Foundation",
] }
ffmpeg-next = "6"
```

---

## Protocolo (protocol.rs)

Todos los mensajes van serializados con `bincode`. Cada mensaje lleva un header de 4 bytes (u32 little-endian) con el tamaño del payload que le sigue.

```rust
#[derive(Serialize, Deserialize, Debug)]
pub enum Message {
    Handshake { version: u8, width: u32, height: u32, fps: u8 },
    VideoFrame { timestamp_ms: u64, data: Vec<u8> },
    MouseMove { x: f32, y: f32 },           // coordenadas normalizadas 0.0-1.0
    MouseClick { button: u8, pressed: bool },
    KeyEvent { keycode: u32, pressed: bool },
    Disconnect,
}
```

Las coordenadas de mouse van normalizadas (0.0 a 1.0) para que el servidor las mapee a su resolución real sin que el cliente necesite saber el tamaño exacto de la pantalla.

---

## UI (ui.rs)

### Estados

```rust
enum AppState {
    Idle,
    Listening,
    Connecting,
    Connected { peer_ip: String },
    Error(String),
}
```

### Layout

Ventana fija 340x220px. Un solo panel central.

```
remotedesk

Tu IP en esta red
192.168.1.45

[ input: IP del host... ]
[ Conectar            ]

● Esperando conexión entrante
```

Cuando está conectado como cliente, la ventana se expande a fullscreen y muestra el framebuffer recibido como textura egui. Un overlay semitransparente en la esquina superior derecha muestra IP conectada y latencia en ms.

### Comportamiento

- Al iniciar la app, arranca el servidor TCP en background (puerto 7070) sin intervención del usuario.
- El input de IP acepta solo caracteres válidos para IPv4 (dígitos y puntos).
- El botón "Conectar" se desactiva si el input está vacío o si ya hay una conexión activa.
- Si la conexión falla, el estado vuelve a `Idle` con un mensaje de error visible por 4 segundos.
- `ctx.request_repaint()` debe llamarse desde los threads de red para forzar repaint cuando cambia el estado.

---

## Módulo macOS (platform/macos.rs)

### Captura (ScreenCaptureKit via FFI)

Usa `SCStreamConfiguration` con:
- `width` y `height` = resolución nativa del display principal (detectada con `CGDisplayBounds` o `NSScreen.main`)
- `minimumFrameInterval = CMTime(1, 120)` para 120fps
- `pixelFormat = kCVPixelFormatType_32BGRA`
- `showsCursor = true`

La resolución detectada se envía al cliente en el `Message::Handshake` para que sepa el tamaño real del framebuffer.

### Encoding (VideoToolbox via FFI)

Crear una `VTCompressionSession` con:
- `kVTCompressionPropertyKey_RealTime = true`
- `kVTCompressionPropertyKey_ProfileLevel = kVTProfileLevel_H264_Baseline_AutoLevel`
- `kVTCompressionPropertyKey_AverageBitRate = 20_000_000` (20 Mbps, ajustable)
- `kVTCompressionPropertyKey_MaxKeyFrameInterval = 120`
- `kVTCompressionPropertyKey_AllowFrameReordering = false`

El callback de output entrega un `CMSampleBuffer`. Extraer con `CMSampleBufferGetDataBuffer` y envolver en `Message::VideoFrame`.

### Inyección de inputs (CoreGraphics)

```rust
// Mouse move
CGEventCreateMouseEvent(null, kCGEventMouseMoved, CGPoint { x, y }, 0)
// Click
CGEventCreateMouseEvent(null, kCGEventLeftMouseDown/Up, pos, kCGMouseButtonLeft)
// Teclado
CGEventCreateKeyboardEvent(null, keycode as CGKeyCode, pressed)
// Post
CGEventPost(kCGHIDEventTap, event)
```

Requiere permiso de Accessibility en System Preferences. Si el permiso no está concedido, loggear el error y retornar `Err` con mensaje claro.

---

## Módulo Windows (platform/windows.rs)

Captura de inputs con hook global de bajo nivel (`SetWindowsHookExW` con `WH_MOUSE_LL` y `WH_KEYBOARD_LL`). Los eventos capturados se serializan como `Message::MouseMove`, `Message::MouseClick`, o `Message::KeyEvent` y se envían por el canal tokio al thread de red.

El cliente Windows no captura pantalla. Solo recibe frames, los decodifica con ffmpeg-next, y los pasa como textura a egui.

---

## Network (network.rs)

### Servidor

```rust
pub async fn start_server(
    port: u16,
    frame_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    input_tx: tokio::sync::mpsc::Sender<Message>,
) -> anyhow::Result<()>
```

- Escucha en `0.0.0.0:7070`.
- Acepta una conexión a la vez. Si llega otra mientras hay una activa, la rechaza con `Message::Disconnect`.
- Al aceptar una conexión, envía `Message::Handshake` con la resolución detectada del display para que el cliente sepa el tamaño del framebuffer.
- Por cada frame recibido en `frame_rx`, lo envuelve en `Message::VideoFrame` y lo escribe al socket.
- Los mensajes de input entrantes los reenvía por `input_tx`.
- Si la conexión se corta, loggear y volver a estado de escucha sin reiniciar la app.

### Cliente

```rust
pub async fn connect(
    host: &str,
    port: u16,
    frame_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    input_rx: tokio::sync::mpsc::Receiver<Message>,
) -> anyhow::Result<()>
```

- Conecta a `host:port`.
- Recibe `Message::Handshake` del servidor con la resolución real del display remoto.
- Lee frames continuamente y los envía por `frame_tx` para que la UI los renderice.
- Lee inputs de `input_rx` y los escribe al socket.
- Si la conexión se corta, reintenta 3 veces con backoff de 1s entre intentos. Si los 3 fallan, retorna `Err`.

### Framing

Cada mensaje en el wire:
```
[4 bytes: tamaño payload u32 LE][payload: bincode bytes]
```

Implementar `async fn write_message(stream: &mut TcpStream, msg: &Message)` y `async fn read_message(stream: &mut TcpStream) -> anyhow::Result<Message>` como helpers internos.

---

## Manejo de errores (runtime)

Usar `anyhow::Result` en todas las funciones que pueden fallar. No usar `unwrap()` ni `expect()` fuera de tests.

Comportamientos de recuperación automática:

| Situación | Comportamiento |
|---|---|
| Conexión TCP cortada (servidor) | Volver a `Listening`, loggear, no crashear |
| Conexión TCP cortada (cliente) | Reintentar 3 veces, luego `Error` en UI |
| Frame de video corrupto | Descartar frame, loggear, continuar |
| Permiso Accessibility denegado (macOS) | Mostrar error en UI con instrucción explícita |
| Puerto 7070 ocupado | Mostrar error en UI, no crashear |
| IP inválida en input | El botón Conectar no debe estar activo, validar antes |

---

## Tests

### Convenciones

- Tests unitarios en el mismo archivo con `#[cfg(test)]`.
- Tests de integración en `tests/`.
- Ningún test hace I/O de red real sin mockear. Usar `tokio::net::TcpListener` en puerto 0 (asignado por OS) para tests de red.
- Los tests de plataforma (macOS/Windows) se marcan con `#[cfg(target_os = "macos")]` o `#[cfg(target_os = "windows")]` según corresponda.

### Cobertura mínima requerida

**protocol.rs**
- `test_message_roundtrip`: serializar cada variante de `Message` con bincode y deserializar, verificar igualdad.
- `test_framing_write_read`: escribir un mensaje con `write_message` a un buffer en memoria, leerlo con `read_message`, verificar que es el mismo.
- `test_large_frame`: frame de video de 500KB serializa y deserializa correctamente.

**network.rs**
- `test_server_accepts_connection`: servidor escucha en puerto 0, cliente se conecta, handshake exitoso.
- `test_server_rejects_second_connection`: con un cliente ya conectado, un segundo cliente recibe `Message::Disconnect`.
- `test_client_reconnect`: simular corte de conexión, verificar que el cliente reintenta 3 veces.
- `test_client_gives_up_after_3_retries`: servidor no disponible, cliente retorna `Err` después de 3 intentos.

**ui.rs**
- `test_ip_validation`: verificar que IPs malformadas no activan el botón conectar.
- `test_state_transitions`: `Idle -> Connecting -> Connected -> Idle` sin panics.

**platform/macos.rs** (solo en target macos)
- `test_input_message_encoding`: crear un `Message::MouseMove` con coordenadas normalizadas, verificar que al mapear a una resolución conocida (ej. 1920x1080) el resultado es correcto.

### Ejemplo de test de red

```rust
#[tokio::test]
async fn test_server_accepts_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let (frame_tx, frame_rx) = mpsc::channel(10);
    let (input_tx, _input_rx) = mpsc::channel(10);

    tokio::spawn(start_server_with_listener(listener, frame_rx, input_tx));

    let result = connect("127.0.0.1", port, frame_tx, mpsc::channel(10).1).await;
    assert!(result.is_ok());
}
```

---

## Orden de implementación

Seguir este orden estrictamente. No avanzar si el paso actual no compila y sus tests no pasan.

1. `Cargo.toml` + `build.rs` + estructura de directorios vacía
2. `protocol.rs` + sus tests
3. `network.rs` + sus tests (sin plataforma, solo mensajes mock)
4. `platform/macos.rs` — captura de pantalla solamente, sin encoding, guardar PNG a disco para verificar
5. `platform/macos.rs` — agregar VideoToolbox encoding, verificar que el .h264 es válido con ffprobe
6. `platform/macos.rs` — agregar inyección de inputs
7. `platform/windows.rs` — decoding con ffmpeg-next, renderizar frame a pantalla
8. `platform/windows.rs` — captura y envío de inputs
9. `ui.rs` + sus tests
10. `main.rs` — integrar todo, tokio runtime, canales entre módulos
11. Test end-to-end manual: conectar Windows a Mac, verificar stream y control de inputs

---

## Notas de compilación

- En macOS, `build.rs` debe linkear `VideoToolbox.framework`, `CoreGraphics.framework`, `CoreFoundation.framework`, y `CoreMedia.framework`.
- En Windows, ffmpeg-next requiere que las DLLs de FFmpeg estén en el PATH o junto al ejecutable. Documentar esto en un README.md al final.
- El proyecto debe compilar sin warnings. Tratar warnings como errores en CI: `RUSTFLAGS="-D warnings"`.

---

## Lo que este proyecto NO hace (fuera de scope)

- Cifrado del stream (no es necesario en LAN doméstica, puede agregarse después con TLS)
- Autenticación por contraseña
- Múltiples clientes simultáneos
- Audio
- Transferencia de archivos
- Soporte Linux
- Relay o acceso remoto fuera de LAN
