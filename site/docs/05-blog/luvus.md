---
title: Luvus
type: blog
date: 2026-10-17
description: Kenapa Luvus jadi fondasi pilus.
---

Luvus adalah Next-Gen mission control for your AI agents.

...yang pada dasarnya adalah terminal multiplexer. 

Pertama kali mendengar Luvus ketika masih bernama Bohay, dan saat itu masih menggunakan [Herdr](https://herdr.dev/).

Yang membuat saya pribadi tertarik dengan Luvus adalah konsep Universal Harness Protocol (UHP) nya
dan berbagai fitur fancy lainnya. 

Herdr mendapatkan pendanaan $6M dari YCombinator. 100 milyar rupiah untuk sebuah terminal multiplexer. Di lain sisi,
[Superlogical](https://superlogical.com) sedang mengembangkan terminal multiplexer juga bernama Rex, yang saat ini masih private beta.

Yang kurang dari tmux sebagai terminal multiplexer adalah integrasi dengan coding agent yang berjalan di terminal. Apakah tmux
perlu $6M untuk mengembangkan fitur tersebut? Saya rasa tidak. 

Selain itu, saya akhir-akhir ini sedang senang sibuk.

Mengembangkan dan merilis 3 fitur baru untuk aplikasi iOS sambil bermain di Dufan? Tentu saja! Dan dilakukan melalui komputer
yang disimpan di saku.  

Dan juga, komputer sehari-hari saya tidak sehebat yang sebelumnya dan kebanyakan pekerjaan saya berada dan berjalan di komputer lain.
Yang sekaligus menjadikan saya cukup selektif dalam menjalankan aplikasi yang akan digunakan. 

Saya sudah mencoba beberapa Agentic Development Environment (ADE) dan yang paling dekat kecocokannya dengan kebutuhan saya hanya 3:

- t3.codes
- Superset
- Cursor

t3.codes dan Cursor dipertimbangkan ulang karena ukurannya yang tidak kecil berikut dengan footprintnya. Superset cukup sempurna,
namun saya tidak berpikir untuk membayar $20/bulan in this economy sebagai wni hanya untuk bisa mengakses aplikasi mobile nya.

Luvus saya rasa sempurna menjadi fondasi karena saya tidak memiliki kapasitas untuk membuat terminal multiplexer saya sendiri. API nya pun saya rasa cukup meskipun relatif terbatas.

Selama 4 hari di minggu kemarin, saya membuat PoC:

- Terminal client untuk berinteraksi dengan aplikasi coding agent via commandline
- Aplikasi companion untuk mengontrol terminal client dari perangkat mobile
- Interface untuk berkomunikasi diantara keduanya

Nama `l3s` dipilih untuk penamaan sementara yang sekarang resmi menjadi pilus.

Hari-hari setelahnya, saya menggunakan pilus setiap kali didepan komputer dan beberapa kali
ketika sedang afk. pilus adalah luvus klien (atau lebih tepatnya luvus wrapper) dengan some fancy
stuff.

Dan tanpa Luvus, pilus tidak akan pernah ada.
