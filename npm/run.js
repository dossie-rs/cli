#!/usr/bin/env node

const { spawnSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const binName = process.platform === "win32" ? "dossiers.exe" : "dossiers";
const bin = path.join(__dirname, binName);

if (!fs.existsSync(bin)) {
	console.error("Dossiers binary is missing. Please reinstall the package.");
	process.exit(1);
}

const result = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });

process.exit(result.status);
