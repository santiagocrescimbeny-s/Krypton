# Krypton — Solana Atomic Launcher & Liquidity Engine

Estado: **Fases 1 y 2 completadas**; **Fase 3** (constructor atómico Create + Buy) en curso en la rama `phase3-atomic-builder`. Ver [`PROGRESS.md`](PROGRESS.md).

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
  builder/ network/ monitor/            andamiaje para las fases 3-5
tests/phase1_tests.rs      pruebas de wallet, zeroize y configuración
tests/phase2_tests.rs      pruebas del motor de metadatos (servidor HTTP simulado)
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

`DRY_RUN` es `true` por defecto: nada se envía a la red salvo que se desactive
explícitamente.
