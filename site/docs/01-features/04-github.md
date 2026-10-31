---
title: GitHub
description: Repositori, issue, pull request, dan workflow run dari akun gh di Mac kamu.
sidebar:
  icon: github
---

Bagian **Repositories** di atas WORKSPACES menampilkan tiga repo. Repo yang kamu pin di Settings muncul lebih dulu, lalu repo yang paling baru di-push.

## Syarat

CLI [gh](https://cli.github.com) harus sudah masuk ke akun GitHub di Mac kamu. pilus memakai akun Mac ini, bukan akun di mesin remote. Permintaan tidak lewat daemon atau SSH.

## Isi

Pilih repo untuk membuka halaman dengan tab **Issues**, **Pull Requests**, dan **Actions**. Dari sana, kamu bisa membuka issue atau PR dan membaca komentarnya. **All Repositories** di menu konteks header menampilkan semua repo, jumlah bintang, dan jumlah issue atau PR yang masih terbuka. Tombol Back kembali ke halaman sebelumnya.

## Cache dan batas

Permintaan GitHub melewati satu cache. Daftar yang masih baru dipakai lagi tanpa permintaan tambahan, sementara daftar REST divalidasi dengan `ETag`. Kalau GitHub membatasi permintaan, pilus menunggu dan menyimpan informasi rate limit di `pilus.db`, jadi statusnya tidak hilang saat aplikasi dibuka ulang. pilus hanya memuat data untuk tab yang sedang terlihat.

## Di tempat lain

- [Aplikasi iOS](/features/ios) membaca issue dan pull request lewat `gh` di mesin yang menyimpan checkout. Jadi ponsel tidak perlu login ke GitHub sendiri, dan hanya bisa melihat apa yang bisa diakses akun di mesin itu.

Kalau jujur, saya mulai mengerjakan integrasi ini hampir impulsif.

## Mematikan

Matikan **GitHub** atau **Repositories** di Settings. Setelah itu pilus tidak mengirim permintaan GitHub.
