# Krypton — Solana Atomic Launcher & Liquidity Engine

Estado: **Fase 1 completada**; **Fase 2** (metadatos IPFS/Pinata) implementada, pendiente de verificación local. Ver [`PROGRESS.md`](PROGRESS.md).

## Estructura

```
Cargo.toml                 workspace + versiones centralizadas
rust-toolchain.toml        rustc 1.97.1 fijado + clippy + rustfmt
.env.example               plantilla de variables de entorno
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

`DRY_RUN` es `true` por defecto: nada se envía a la red salvo que se desactive
explícitamente.
