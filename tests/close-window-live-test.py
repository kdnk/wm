#!/usr/bin/env python3
# Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
"""Verify native whole-window closing against a disposable, explicitly targeted app."""

import argparse
import json
from pathlib import Path
import subprocess
import tempfile
import time
import uuid


FIXTURE = r'''
import AppKit
import CoreGraphics
import Foundation

let directory = URL(fileURLWithPath: CommandLine.arguments[1])
let title = CommandLine.arguments[2]

func write(_ name: String, _ value: [String: Any]) {
    let data = try! JSONSerialization.data(withJSONObject: value)
    try! data.write(to: directory.appendingPathComponent(name), options: .atomic)
}

class Fixture: NSObject, NSApplicationDelegate, NSWindowDelegate {
    var window: NSWindow!
    let tabs = NSTabView()
    var commandW = 0

    func applicationDidFinishLaunching(_ notification: Notification) {
        let session = CGSessionCopyCurrentDictionary() as? [String: Any]
        if session?["CGSSessionScreenIsLocked"] as? Bool == true {
            write("error.json", ["error": "the desktop is locked"])
            NSApp.terminate(nil)
            return
        }
        window = NSWindow(contentRect: NSRect(x: 100, y: 100, width: 420, height: 240),
                          styleMask: [.titled, .closable, .miniaturizable, .resizable],
                          backing: .buffered, defer: false)
        window.title = title
        window.isReleasedWhenClosed = false
        window.delegate = self
        for name in ["First tab", "Second tab"] {
            let item = NSTabViewItem(identifier: name)
            item.label = name
            item.view = NSTextField(labelWithString: name)
            tabs.addTabViewItem(item)
        }
        window.contentView = tabs
        let menu = NSMenu()
        let application = NSMenuItem()
        let actions = NSMenu()
        let close = NSMenuItem(title: "Close Tab", action: #selector(closeTab), keyEquivalent: "w")
        close.target = self
        actions.addItem(close)
        application.submenu = actions
        menu.addItem(application)
        NSApp.mainMenu = menu
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) {
            write("ready.json", ["pid": ProcessInfo.processInfo.processIdentifier,
                                 "tabs": self.tabs.numberOfTabViewItems])
        }
    }

    @objc func closeTab() {
        commandW += 1
        if let tab = tabs.selectedTabViewItem { tabs.removeTabViewItem(tab) }
    }

    func windowWillClose(_ notification: Notification) {
        write("closed.json", ["closed": true, "tabs": tabs.numberOfTabViewItems,
                              "command_w": commandW])
        DispatchQueue.main.async { NSApp.terminate(nil) }
    }
}

let application = NSApplication.shared
application.setActivationPolicy(.regular)
let fixture = Fixture()
application.delegate = fixture
application.run()
'''


def wait_json(directory, name, process):
    deadline = time.monotonic() + 10
    path = directory / name
    while time.monotonic() < deadline:
        if path.exists():
            return json.loads(path.read_text())
        error = directory / "error.json"
        if error.exists():
            raise AssertionError(json.loads(error.read_text())["error"])
        if process.poll() is not None:
            raise AssertionError(f"Fixture exited before producing {name}")
        time.sleep(0.05)
    raise AssertionError(f"Fixture did not produce {name}")


def main():
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--checker", type=Path,
                        default=root / "target/debug/examples/close-window-check")
    args = parser.parse_args()
    if not args.checker.is_file():
        parser.error("build the checker with cargo build --locked --example close-window-check")
    with tempfile.TemporaryDirectory(prefix="wm-close-window-") as scratch:
        directory = Path(scratch)
        source = directory / "main.swift"
        source.write_text(FIXTURE)
        binary = directory / "CloseFixture"
        subprocess.run(["swiftc", "-module-cache-path", str(directory / "modules"),
                        str(source), "-o", str(binary)], check=True)
        title = "wm close fixture " + uuid.uuid4().hex
        process = subprocess.Popen([str(binary), str(directory), title])
        try:
            ready = wait_json(directory, "ready.json", process)
            assert ready == {"pid": process.pid, "tabs": 2}, ready
            subprocess.run([str(args.checker.resolve()), str(process.pid), title],
                           check=True, timeout=5)
            result = wait_json(directory, "closed.json", process)
            assert result == {"closed": True, "tabs": 2, "command_w": 0}, result
            process.wait(timeout=5)
            assert process.returncode == 0, process.returncode
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=5)
    print("PASS: native close button closes a two-tab fixture window without Command-W")


if __name__ == "__main__":
    main()
