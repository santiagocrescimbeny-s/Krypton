# Krypton — Solana Atomic Launcher & Liquidity Engine

Estado: **Fase 1** (arquitectura base, seguridad de memoria y entorno).

## Estructura

```
Cargo.toml                 workspace + versiones centralizadas
rust-toolchain.toml        stable + clippy + rustfmt
.env.example               plantilla de variables de entorno
src/
  lib.rs                   lints "zero panics" y declaración de módulos
  errors.rs                LauncherError / ConfigError / WalletError (thiserror)
  parsing.rs               parsers puros (SOL -> lamports, bool, byte array)
  config.rs                Config tipada desde entorno (dotenvy + serde)
  security/
    secret.rs              SecretString (ZeroizeOnDrop, Debug redactado)
    wallet.rs              SecureWallet + SecretKeyBytes (ZeroizeOnDrop)
  storage/ builder/ network/ monitor/   andamiaje para las fases 2-5
tests/phase1_tests.rs      pruebas de wallet, zeroize y configuración
```

## Puerta de calidad de la fase

```bash
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
```

## Configuración

```bash
cp .env.example .env   # y rellenar PRIVATE_KEY, CREATOR_SOL_AMOUNT, etc.
```

`DRY_RUN` es `true` por defecto: nada se envía a la red salvo que se desactive
explícitamente.
