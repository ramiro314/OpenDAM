# OpenDAM

A personal, Linux-only desktop asset manager for game-dev assets: glTF/FBX/OBJ models and PNG
sprites and textures. It indexes asset folders into per-library SQLite databases, renders
thumbnails, and offers tags, collections, full-text search and a 3D viewer. It is built with
Tauri v2 (Rust), React and a Python preview worker.

This is my fork, maintained mostly by coding agents. Agent instructions live in
[AGENTS.md](AGENTS.md).

## Setup and run

You need [mise](https://mise.jdx.dev) and Tauri's
[Linux system dependencies](https://v2.tauri.app/start/prerequisites/#linux) (webkit2gtk-4.1 and
friends). mise installs bun, rust, uv and the rest of the pinned toolchain.

From a checkout:

```sh
mise install && make deps && make dev
```

The first `make dev` compiles the Rust backend, which takes several minutes. Then click **Add
Library** and choose a folder of assets.

## Credit and license

OpenDAM was created by [LawLaw09/OpenDAM](https://github.com/LawLaw09/OpenDAM). This fork keeps
its MIT license: Copyright (c) 2026 OpenDAM Contributors. See [LICENSE](LICENSE) for the full text.
