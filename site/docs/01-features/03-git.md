---
title: Git
description: Review perubahan, lihat riwayat, dan stage per file.
sidebar:
  icon: git-branch
---

Kedua tab ini mengikuti workspace yang dipilih di sidebar. Daemon yang menjalankan Git; pilus hanya
menggambar jawabannya, dan tidak pernah menulis ke checkout.

## Diff

Review hanya-baca atas perubahan workspace aktif.

- Daftar file perubahan di atas atau di samping patch, tergantung lebar.
- **Catatan review** pada baris atau blok. Catatan disimpan daemon dan bisa dikirim ke agent.
- **Stage/unstage per file**. Ini satu-satunya tulisan yang dilakukan pilus, dan hanya ke index.
- File markdown bisa dilihat sebagai dokumen yang sudah dirender.

## Git

Riwayat commit di atas; klik sebuah commit untuk melihat apa yang diubahnya di bawah. Perubahan
yang belum di-commit ada di tab Diff.

## Worktree

Menu konteks workspace bisa membuat worktree baru dan menghapus worktree. Menghapus worktree yang
tertaut meminta konfirmasi.
