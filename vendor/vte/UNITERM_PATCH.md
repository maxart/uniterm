# Uniterm VTE patch

Uniterm vendors crates.io vte 0.13.1 with an on-demand, serde-enabled parser checkpoint.
The checkpoint captures and checks the existing parameter, intermediate, OSC and partial UTF-8 state.
The parser layout, byte processing and performer callbacks remain upstream code.
Uniterm exercises every split point and malformed checkpoint ranges in its terminal tests.
