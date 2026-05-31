# Embedded Control Hub (STM32MP157F-DK2)

Welcome to the central repository for the Universal Hardware Controller project. This platform leverages an Asymmetric Multiprocessing (AMP) architecture on the STM32MP157F-DK2 Discovery Kit, integrating a high-performance **Rust application layer** running on a custom **Yocto-built Linux OS (ARM Cortex-A7)** alongside deterministic **real-time firmware (ARM Cortex-M4)**.

This repository serves as both a production-intent prototype framework and the final project submission for the Coursera Advanced Embedded Linux/Interface Design curriculum series.

---

## 🔗 Centralized Documentation & Sprints (Peer-Review Portal)

To comply with the individual and group evaluation rubrics, all master system specifications, architectural components, and active execution lifecycles have been centralized within the repository's native GitHub infrastructure. 

Please utilize the direct engineering links below to evaluate the submission deliverables:

* 🚀 **Main Documentation Entry Point:** [Project Overview Wiki Page](https://github.com/CiprianTiro/stm32mp1-universal-controller/wiki)
    * *Contains: Project goals, comprehensive system architecture diagrams, target Yocto build specifications, multi-language source organization maps, and course dependency breakdowns.*
* 🗓️ **Milestones & Sprints Table:** [Project Schedule & Timelines Page](https://github.com/CiprianTiro/stm32mp1-universal-controller/wiki/Project-Schedule)
    * *Contains: Active sprint boundaries, target milestone completion windows, task allocations, and individual contribution mappings.*
* 📊 **Live Execution Tracking Board:** [Agile Sprint Kanban Platform](https://github.com/users/CiprianTiro/projects/1/views/1)
    * *Contains: Publicly accessible task pipelines (Todo / In Progress / Done) featuring granular Definitions of Done (DoD) for active Sprint items.*

---

## 📂 Source Code Layout

```text
stm32mp1-universal-controller/
├── yocto_layers/     # Custom distribution bitbake configuration layers (meta-rust, meta-app)
├── firmware_m4/      # Real-time coprocessor control logic workspace (C/FreeRTOS infrastructure)
├── linux_a7/         # Core application stack (Asynchronous Rust backend daemon + Slint GUI UI)
└── README.md         # Navigation entry point
```

## ⚖️ Intellectual Property & License

This project is licensed under the terms of the **GNU General Public License v3.0 (GPLv3)**.

### 🛡️ Core Provisions & Copyleft Protection:
* **Commercial Use Permitted:** You are legally permitted to use, modify, and distribute this software for private prototypes, research, and commercial systems.
* **The Copyleft Requirement (Strong Protection):** Anyone who modifies this code and distributes a product using it **must make their entire modified source code public** under the exact same GPLv3 license. A company cannot legally steal your Yocto layers, Rust daemons, or C firmware, make them proprietary, and sell them in secret.
* **As-Is Liability Protection:** The software is provided without warranty of any kind. You cannot be held legally or financially responsible if someone experiences system instability, property damage, or hardware issues while using this code.
* **Disclose Source & License Notice:** Anyone distributing binaries built from this project must explicitly include the original copyright notice, a copy of this GPLv3 license text, and a clear method for users to download the source code.

*For full details regarding the formal legal text, permissions, and reciprocal conditions, please consult the local `LICENSE` file sitting at the root directory of this repository.*