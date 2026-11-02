---
title: Git
description: Review perubahan, kirim catatan ke agent, lihat riwayat, dan stage per file.
sidebar:
  icon: git-branch
---

Tab **Diff** dan **Git** mengikuti workspace yang dipilih di sidebar. Keduanya hanya aktif di repositori Git. Daemon menjalankan Git dan mengirim hasilnya; pilus menampilkannya. Checkout hanya berubah kalau kamu menekan tombol yang tercantum di bagian [Yang ditulis pilus](#yang-ditulis-pilus).

## Diff

Di sini kamu bisa meninjau perubahan di workspace aktif. Saya masih sering membuka tab ini untuk code review, paling tidak supaya bisa meninggalkan catatan buat agent.

- Daftar file perubahan muncul di atas atau di samping patch, tergantung lebar jendela. Kamu bisa melihatnya sebagai daftar datar atau pohon folder, lalu menyaringnya berdasarkan nama.
- Patch mendapat syntax highlighting.
- **Catatan review** bisa ditempelkan ke baris atau blok. Catatan disimpan oleh daemon dan bisa dikirim ke agent. Kartu catatan dan kolom tulisnya selebar baris supaya teksnya tidak terpotong.
- **Viewed** menandai file yang sudah kamu baca, jadi sisanya lebih mudah ditemukan.
- File markdown bisa dibaca sebagai dokumen yang sudah dirender. **Changed** hanya menampilkan blok yang berubah dan melipat sisanya. **Full** menampilkan seluruh dokumen, sedangkan **Diff** kembali ke patch. Penanda hijau dan merahnya sama seperti di [editor](/features/editor).

## Git

Tab ini menampilkan riwayat commit, dengan branch aktif di chip berbingkai di header (Diff memakai chip yang sama). Pilih commit untuk melihat perubahannya. Riwayat dimuat sedikit demi sedikit saat kamu menggulir, jadi repo dengan ribuan commit tetap ringan. Perubahan yang belum di-commit ada di tab Diff.

## Worktree

Menu konteks workspace bisa membuat atau menghapus worktree. Penghapusan perlu dikonfirmasi. Kalau masih ada perubahan yang belum di-commit atau ada proses yang memakai worktree itu, pilus menjelaskan alasannya dan menahan penghapusan. Konfirmasi kedua baru membuang pekerjaan yang belum di-commit. Kalau pilus tidak bisa membaca status worktree, penghapusan dibatalkan, bukan dianggap bersih. Di sidebar, worktree muncul di bawah workspace induknya.

## Yang ditulis pilus

Inilah satu-satunya tindakan yang mengubah checkout, dan semuanya dimulai dari tombol yang kamu tekan:

| Aksi | Efek |
| --- | --- |
| Stage / unstage per file | Mengubah index. |
| Discard per file | Membuang perubahan di working tree. pilus selalu bertanya dulu. Tindakan ini tidak bisa dibatalkan. File yang sudah di-stage tidak punya tombol Discard; unstage dulu. |
| Hapus worktree | Memerlukan konfirmasi. Perubahan yang belum di-commit baru dibuang setelah konfirmasi kedua. |
