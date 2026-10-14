const LOCAL_HOSTS = new Set(["localhost", "127.0.0.1", "[::1]"]);

export function isInsecureOrigin(protocol: string, hostname: string) {
	return protocol === "http:" && !LOCAL_HOSTS.has(hostname);
}
