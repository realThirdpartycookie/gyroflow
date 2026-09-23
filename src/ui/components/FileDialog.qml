// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright © 2022 Adrian <adrian.eddy at gmail>

import QtQuick
import QtQuick.Controls as QQC
import QtQuick.Dialogs

import "../Util.js" as Util;

FileDialog {
    id: root;
    property string type: "";
    // In a `Connections` and not in an `onAccepted` handler, because a handler declared where the dialog is used would override one declared here
    Connections {
        target: root;
        function onAccepted(): void { settings.setValue("folder-" + root.type, filesystem.get_folder(root.selectedFile).toString()); }
    }

    // Browser build: Qt's dialog would only browse the in-memory FS, so open files through the browser's picker
    property bool webPicking: false;
    Connections {
        target: filesystem;
        enabled: root.webPicking;
        function onWeb_files_picked(urls: var): void {
            root.webPicking = false;
            if (urls.length) { root.selectedFile = urls[0]; root.accepted(); } else { root.rejected(); }
        }
    }

    function open2(): void {
        if (Qt.platform.os == "wasm" && root.fileMode != FileDialog.SaveFile) {
            const exts = (root.nameFilters || []).join(" ").match(/\*\.\w+/g) || [];
            root.webPicking = true;
            filesystem.web_pick_files([...new Set(exts.map(x => x.substring(1).toLowerCase()))].join(","), root.fileMode == FileDialog.OpenFiles);
            return;
        }
        const savedFolder = settings.value("folder-" + type, "");
        if (savedFolder) currentFolder = savedFolder;
        open();
    }
}
