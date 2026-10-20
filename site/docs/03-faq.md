---
title: FAQ
description: Pertanyaan yang paling sering muncul.
sidebar:
  icon: circle-help
---

## Apakah pilus menggantikan Luvus?

Tidak. Luvus adalah daemon yang memegang sesi, workspace, dan agent. pilus hanya klien yang
menggambar permukaannya di jendela native. Tanpa Luvus, pilus tidak punya apa-apa untuk ditampilkan.

## Apa yang terjadi kalau jendela ditutup atau Mac tidur?

Luvus tetap berjalan, jadi agent juga. pilus tinggal terhubung lagi. Koneksi ke mesin SSH yang
putus dicoba ulang otomatis (jeda 1 detik, berlipat sampai 30 detik) tanpa mengosongkan layar.

## Kenapa tidak pakai terminal emulator saja?

Daemon mengirim permukaannya sebagai grid sel bergaya, jadi pilus tidak perlu emulator terminal
lain. Sidebar, tab, dan panel kanan digambar native, bukan dari sel terminal.

## Platform apa yang didukung?

macOS di Apple Silicon adalah platform utama. iOS dan iPadOS lewat TestFlight. GNU/Linux belum ditentukan.

## Seberapa berat pilus?

Target:

- 350 mCPU (150 mCPU idle)
- ~60MB RSS

## Apakah pilus mengirim data ke GitHub atau server lain?

Bagian Repositories memakai `gh` di Mac kamu sendiri, tidak lewat daemon atau SSH, dan semua
permintaan di-cache. Matikan di Settings dan tidak ada permintaan sama sekali. Selain itu pilus
hanya mengambil manifest pembaruan dari `releases.kepelet.com`.

## Di mana pengaturan disimpan?

Di `settings.json` pada folder state pilus. Catatan pada markdown ada di `md-notes.json`,
cache GitHub di `github-cache.json`, di folder yang sama.

## Aplikasi iOS aman?

Ponsel dipasangkan lewat kode sekali pakai yang hanya tampil di host, lalu dikenali dari
identitas iroh-nya. Tiap perangkat punya level View, Control, atau Operate dan bisa dicabut kapan saja.
Detailnya di [aplikasi iOS](/features/ios).
