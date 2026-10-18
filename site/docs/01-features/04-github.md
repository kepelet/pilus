---
title: GitHub
description: Repositori, issue, pull request, dan workflow run dari akun gh di Mac kamu.
sidebar:
  icon: github
---

Bagian **Repositories** di atas WORKSPACES menampilkan tiga repositori: yang kamu pin di Settings
lebih dulu, lalu yang paling baru di-push.

## Syarat

CLI [`gh`](https://cli.github.com) sudah login di Mac. Akun yang dipakai adalah akun Mac ini,
bukan akun mesin remote, dan tidak lewat daemon atau SSH.

## Isi

Klik baris untuk membuka halaman repositori dengan tab **Issues**, **Pull Requests**, dan
**Actions**. Dari sana kamu bisa membuka satu issue atau PR beserta komentarnya. **All Repositories**
di menu klik-kanan header membuka daftar lengkap dengan jumlah bintang dan yang masih terbuka.
Back kembali selangkah ke belakang.

## Cache dan batas

Semua permintaan lewat satu cache: daftar yang masih segar tidak meminta ulang, daftar REST
divalidasi dengan `ETag`, dan rate limit ditunggu lalu disimpan di `github-cache.json`. Hanya tab
yang sedang terlihat yang dibaca.

## Mematikan

Matikan **GitHub** atau **Repositories** di Settings dan tidak ada permintaan sama sekali.
