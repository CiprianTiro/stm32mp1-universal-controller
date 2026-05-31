## 🚚 Workspace Initialization (For New Developers)

This repository tracks external Yocto and hardware vendor layers as **Git Submodules**. This ensures the correct versions are locked down without breaking main source history.

If you are cloning this repository for the first time, use the `--recurse-submodules` flag to automatically pull down all the linked Yocto ecosystem dependencies in a single action:

```bash
git clone --recurse-submodules https://github.com/CiprianTiro/stm32mp1-universal-controller
```