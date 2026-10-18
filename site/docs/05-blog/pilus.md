---
title: pilus
type: blog
date: 2026-10-17
description: "Pi, Iroh, dan Luvus: tumpukan teknologi di balik pilus."
---

pilus adalah singkatan dari **P**i, **I**roh, **L**uv**us**:

- Pi adalah agent harness untuk pilus
- Iroh adalah network stack untuk koneksi P2P
- Luvus adalah terminal multiplexer

Tiga teknologi diatas adalah tumpukan yang pas untuk kebutuhan saya dalam membuat ADE yang saya
inginkan. 

## Fitur

- Antarmuka grafis untuk Luvus
- Lightweight dibangun diatas GPUI
- In-app Browser via wry (WebKit di macOS)
- Port Forwarder
- Git integration
- File explorer

## Prinsip

- Low footprint (~60MB RSS)
- Performant (Metal backend di macOS berkat GPUI)
- Lean (baseline nya adalah Macbook Neo)

## Platform

- CPU arsitektur: aarch64 & x86_64
- GPU backend: Metal & wgpu
- OS:
  - macOS first (daily driver)
  - GNU/Linux (TBD)
  - iOS & iPadOS (TestFlight)
