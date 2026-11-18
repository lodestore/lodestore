import { Box, Database, Folder, type LucideIcon, Monitor } from "lucide-react";
import { copy } from "@/copy";

export const SOURCE_TYPES = [
	"files",
	"proxmox-vm",
	"proxmox-ct",
	"postgres",
	"mysql",
] as const;

export type SourceType = (typeof SOURCE_TYPES)[number];

export const sourceTypeIcon: Record<SourceType, LucideIcon> = {
	files: Folder,
	"proxmox-vm": Monitor,
	"proxmox-ct": Box,
	postgres: Database,
	mysql: Database,
};

const labels: Record<SourceType | "postgres-wal", string> = {
	files: copy.sources.type.files,
	"proxmox-vm": copy.sources.type.proxmoxVm,
	"proxmox-ct": copy.sources.type.proxmoxCt,
	postgres: copy.sources.type.postgres,
	"postgres-wal": copy.sources.type.postgresWal,
	mysql: copy.sources.type.mysql,
};

export const sourceTypeLabel = (type: string): string =>
	labels[type as keyof typeof labels] ?? type;

const nouns: Record<string, string> = {
	"proxmox-vm": copy.sources.noun.proxmoxVm,
	"proxmox-ct": copy.sources.noun.proxmoxCt,
	postgres: copy.sources.noun.postgres,
	mysql: copy.sources.noun.mysql,
};

export const sourceTypeNoun = (type: string): string | undefined => nouns[type];

export const parseSourceType = (value: unknown): SourceType | undefined =>
	SOURCE_TYPES.find((type) => type === value);
