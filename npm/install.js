const os = require("os");
const path = require("path");
const fs = require("fs");
const https = require("https");

const platform = `${os.platform()}-${os.arch()}`;
const version = require("./package.json").version;
const targets = {
	"linux-x64": "dossiers-linux-x86_64",
	"linux-arm64": "dossiers-linux-aarch64",
	"darwin-x64": "dossiers-macos-x86_64",
	"darwin-arm64": "dossiers-macos-aarch64",
	"win32-x64": "dossiers-windows-x86_64.exe",
	"win32-arm64": "dossiers-windows-aarch64.exe",
};

const target = targets[platform];
if (!target) {
	console.error(`Unsupported platform: ${platform}`);
	process.exit(1);
}

const downloadUrl = `https://github.com/dossie-rs/cli/releases/download/v${version}/${target}`;
const binName = os.platform() === "win32" ? "dossiers.exe" : "dossiers";
const binPath = path.join(__dirname, binName);

if (fs.existsSync(binPath)) {
	process.exit(0);
}

const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "dossiers-"));
const downloadPath = path.join(tmpDir, target);

function download(url, destination) {
	return new Promise((resolve, reject) => {
		const file = fs.createWriteStream(destination);
		const handleResponse = (res) => {
			if (
				res.statusCode >= 300 &&
				res.statusCode < 400 &&
				res.headers.location
			) {
				// Follow redirects for GitHub release assets
				return download(res.headers.location, destination)
					.then(resolve)
					.catch(reject);
			}

			if (res.statusCode !== 200) {
				return reject(
					new Error(`Download failed with status ${res.statusCode}`),
				);
			}

			res.pipe(file);
			file.on("finish", () => file.close(resolve));
		};

		https.get(url, handleResponse).on("error", (err) => {
			fs.rm(destination, { force: true }, () => reject(err));
		});
	});
}

(async () => {
	try {
		console.log(`Downloading ${target}...`);
		await download(downloadUrl, downloadPath);

		fs.copyFileSync(downloadPath, binPath);
		fs.chmodSync(binPath, 0o755);
		console.log(`Installed binary to ${binPath}`);
	} catch (err) {
		console.error(err.message || err);
		process.exit(1);
	} finally {
		fs.rm(tmpDir, { recursive: true, force: true }, () => {});
	}
})();
