import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

export const limitKeywords = [
	/Windows/,
	/macOS/,
	/Proxmox/,
	/Kubernetes/,
	/database/i,
	/RBAC|roles/i,
	/single sign-on|SSO/i,
	/SFTP|WebDAV/,
];

export function limitTitles() {
	const md = readFileSync(fileURLToPath(new URL('../src/content/pages/roadmap.md', import.meta.url)), 'utf8');
	const front = md.split(/^---$/m)[1] ?? '';
	return [...front.matchAll(/^\s*-\s+title:\s*(.+)$/gm)].map((m) => m[1].trim().replace(/^(["'])(.*)\1$/, '$2'));
}
