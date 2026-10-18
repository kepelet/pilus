---
title: File Explorer
description: Pohon file workspace di panel kanan, dengan tanda perubahan Git.
sidebar:
  icon: folder-tree
---

Tab **Files** menampilkan pohon yang sama dengan TUI Luvus untuk workspace yang dipilih.

## Memakainya

- Klik file untuk membukanya di kartu terminal (lewat daemon).
- Lipat dan buka folder. Lipatan hanya milik jendela ini dan diingat per pohon, jadi pindah
  workspace lalu kembali tidak membukanya lagi.
- File yang berubah dan folder di atasnya diberi tanda; klik file yang berubah membuka Diff-nya.
- Klik kanan untuk membuat, mengganti nama, dan menghapus entri.

## Markdown

File markdown tanpa perubahan Git dibaca pilus sendiri dan digambar sebagai dokumen. Pilih teksnya
untuk menyalin, atau kirim kutipan ke agent yang sedang berjalan. Pilihan juga bisa jadi **catatan**
dengan permintaanmu; kumpulkan beberapa, lalu kirim sekaligus sebagai satu `agent.prompt`.
Catatan disimpan per mesin dan file di `md-notes.json`, bukan di review daemon.

## Unggah dan unduh

Seret file atau folder ke tab Files untuk mengunggahnya; pilus menunjukkan di mana ia mendarat.
Unduh dari menu klik-kanan.
