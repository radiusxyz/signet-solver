# TODO Backlog

## Core Engine & Configuration
- **crates/solver-core/src/engine/mod.rs:237** – Make recovery failure handling configurable (continue vs abort).
- **crates/solver-core/src/engine/cost_profit.rs:112** – Include order standard when building quote requests.
- **crates/solver-core/src/engine/cost_profit.rs:417** – Integrate configurable live gas estimation instead of the current stub.
- **crates/solver-core/src/engine/token_manager.rs:111** – Re-enable automatic token approval management during startup.
- **crates/solver-config/src/lib.rs:593** – Restore API configuration validation once the feature set is ready.

## Delivery (Signet)
- **crates/solver-delivery/src/implementations/signet/bundle.rs:699-701** – Add cost/gas-aware route selection instead of picking the first viable path.
- **crates/solver-delivery/src/implementations/signet/bundle.rs:912** – Implement bundle confirmation tracking.
- **crates/solver-delivery/src/implementations/signet/bundle.rs:924** – Implement bundle receipt retrieval.
- **crates/solver-delivery/src/implementations/signet/bundle.rs:941** – Implement host balance queries for Signet delivery.
- **crates/solver-delivery/src/implementations/signet/bundle.rs:954** – Implement allowance queries for Signet delivery.
- **crates/solver-delivery/src/implementations/signet/bundle.rs:1008** – Implement generic contract call support if Signet delivery requires it.

## API & Discovery
- **crates/solver-service/src/apis/quote/generation.rs:184** – Support quotes with multiple destination chains.
- **crates/solver-service/src/apis/quote/signing/payloads/permit2.rs:41** – Handle multi-input/multi-output Permit2 payloads.
- **crates/solver-service/src/apis/order.rs:121/202/253** – Parse SignedOrder data to expose accurate input/output amounts and support multiple entries.
- **crates/solver-service/src/apis/order.rs:302** – Add support for settlement types beyond the current default.
- **crates/solver-discovery/src/implementations/offchain/_7683.rs:523** – Populate `quote_id` when constructing intents from off-chain discovery.

## Order Processing
- **crates/solver-order/src/implementations/standards/signet.rs:116** – Allow the Signet rollup chain ID to be configurable.

## DEX & Pricing
- **crates/solver-dex/src/router.rs:373** – Provide dynamic gas estimation for swap routes.
- **crates/solver-dex/src/uniswap_v4.rs:82** – Implement on-chain pool discovery rather than relying on static config.

