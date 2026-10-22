# Krypton — Solana Atomic Launcher & Liquidity Engine

Estado: **Fases 1-3 completadas** (base, metadatos IPFS/Pinata, constructor atómico Create + Buy), consolidadas en `main`; Fase 4 (Jito) pendiente. Ver [`PROGRESS.md`](PROGRESS.md).

## Estructura

```
Cargo.toml                 workspace + versiones centralizadas
rust-toolchain.toml        rustc 1.97.1 fijado + clippy + rustfmt
.env.example               plantilla de variables de entorno
.github/workflows/ci.yml   CI: fmt, check, clippy -D warnings, test, cargo audit, gitleaks
src/
  lib.rs                   lints "zero panics" y declaración de módulos
  errors.rs                LauncherError / ConfigError / WalletError / IpfsError (thiserror)
  parsing.rs               parsers puros (SOL -> lamports, bool, byte array)
  config.rs                Config tipada desde entorno (dotenvy + serde)
  security/
    secret.rs              SecretString (ZeroizeOnDrop, Debug redactado)
    wallet.rs              SecureWallet + SecretKeyBytes (ZeroizeOnDrop)
  storage/metadata.rs      cliente Pinata/IPFS: imagen + JSON Metaplex/Pump.fun -> metadata_uri
  builder/                 Fase 3: constants, pda, math, instructions, transaction (Create + Buy atómico)
  network/ monitor/        andamiaje para las fases 4-5
tests/phase1_tests.rs      pruebas de wallet, zeroize y configuración
tests/phase2_tests.rs      pruebas del motor de metadatos (servidor HTTP simulado)
tests/phase3_tests.rs      pruebas de PDAs, curva, instrucciones, transacción y guardas de gasto
tests/env_hygiene_tests.rs higiene del entorno (.env explícito, PRIVATE_KEY limpiada)
tests/common/mod.rs        servidor HTTP de pruebas (solo std)
```

## Puerta de calidad de la fase

```bash
cargo check --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

## Configuración

```bash
cp .env.example .env   # y rellenar PRIVATE_KEY, CREATOR_SOL_AMOUNT, etc.
```

El `.env` se lee **solo** desde `./.env` (directorio de trabajo) o desde la ruta dada a
`Config::from_env_file`; nunca se buscan directorios padre. `PRIVATE_KEY` se elimina del
entorno del proceso nada más leerla y `Config` conserva únicamente el `SecureWallet` ya parseado.

Topes de gasto (validados al arrancar y de nuevo en el constructor):

- `SLIPPAGE_BPS` ≤ 1000 (10 %).
- `PRIORITY_FEE_MICRO_LAMPORTS × COMPUTE_UNIT_LIMIT / 1e6` ≤ `MAX_PRIORITY_FEE_LAMPORTS`
  (por defecto 5 000 000 lamports; techo absoluto 50 000 000).
- Antes de ensamblar la transacción, `LaunchBuilder::ensure_funded` / `build_signed_checked` exigen
  que el pagador tenga el coste máximo (`LaunchParams::required_balance`).

`Config::from_env()` modifica el entorno del proceso (`remove_var`): llámalo desde un `fn main()` síncrono
**antes** de construir el runtime de Tokio.

`DRY_RUN` es `true` por defecto: nada se envía a la red salvo que se desactive
explícitamente.
