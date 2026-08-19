/**
 * The client half of TUS, by hand.
 *
 * `tus-js-client` exists and is not used, for one reason: the protocol's client side is a create request
 * and a loop of `PATCH`es, and the part that matters — **resuming from the server's offset rather than
 * from the client's idea of it** — is four lines. A dependency would hide those four lines, and they are
 * the ones that decide whether a 4 GB upload survives a dropped connection.
 *
 * ## The offset comes from the server, always
 *
 * After any failure the next `PATCH` starts at whatever `HEAD` reports, not at what this code last sent.
 * A client that trusted its own counter would re-send bytes the server already has (harmless, slow) or
 * skip bytes it does not (corrupt, silent) — and the second is only visible when someone opens the file.
 */

/** How much to send per request. */
export const CHUNK_BYTES = 8 * 1024 * 1024;

/** The version this client speaks. The server refuses anything else with a 412. */
const TUS_VERSION = '1.0.0';

export type Progress = {
	sent: number;
	total: number;
};

export type UploadHandle = {
	uploadId: string;
	location: string;
};

/**
 * Base64 of a UTF-8 string, as `Upload-Metadata` requires.
 *
 * `btoa` is Latin-1 only, so a filename with an em dash or an accent throws. The round trip through
 * `TextEncoder` is what makes a non-ASCII filename uploadable at all — and in a DAM those are the normal
 * case, not an edge one.
 */
export function encodeMetadata(value: string): string {
	const bytes = new TextEncoder().encode(value);
	let binary = '';
	for (const byte of bytes) binary += String.fromCharCode(byte);
	return btoa(binary);
}

/** Creates an upload session and returns where to send bytes. */
export async function create(
	base: string,
	key: string,
	file: { name: string; size: number; type: string },
	/**
	 * The upload profile's *key*, when one was chosen.
	 *
	 * By key rather than id, matching the server: a client that knows its intake by name should not have to
	 * look one up first. Omitted entirely when absent, because an empty `profile` value would be a *named*
	 * profile that resolves to nothing — a different and worse answer than not naming one.
	 */
	profile?: string
): Promise<UploadHandle> {
	const metadata = [
		`filename ${encodeMetadata(file.name)}`,
		`filetype ${encodeMetadata(file.type || 'application/octet-stream')}`
	];
	if (profile) {
		metadata.push(`profile ${encodeMetadata(profile)}`);
	}
	const response = await fetch(`${base}/uploads`, {
		method: 'POST',
		headers: {
			Authorization: `Bearer ${key}`,
			'Tus-Resumable': TUS_VERSION,
			'Upload-Length': String(file.size),
			'Upload-Metadata': metadata.join(',')
		}
	});
	if (!response.ok) {
		throw new Error(`could not start the upload (${response.status})`);
	}
	const location = response.headers.get('location');
	if (!location) {
		// The protocol requires it, and without it there is nowhere to send bytes. Failing loudly beats
		// guessing a URL from the id.
		throw new Error('the server did not say where to send the bytes (no Location header)');
	}
	return { uploadId: location.split('/').pop() ?? '', location };
}

/** The server's current offset for an upload. The only source of truth — see the module docs. */
export async function offsetOf(base: string, key: string, location: string): Promise<number> {
	const response = await fetch(absolute(base, location), {
		method: 'HEAD',
		headers: { Authorization: `Bearer ${key}`, 'Tus-Resumable': TUS_VERSION }
	});
	if (!response.ok) {
		throw new Error(`could not read the upload offset (${response.status})`);
	}
	const offset = Number(response.headers.get('upload-offset'));
	return Number.isFinite(offset) ? offset : 0;
}

/**
 * Sends `file` to an existing session, resuming wherever the server is.
 *
 * `signal` aborts between chunks as well as within one, so cancelling a 4 GB upload does not wait for the
 * current 8 MB to finish.
 */
export async function send(
	base: string,
	key: string,
	handle: UploadHandle,
	file: Blob,
	onprogress: (progress: Progress) => void,
	signal?: AbortSignal
): Promise<void> {
	let sent = await offsetOf(base, key, handle.location);
	onprogress({ sent, total: file.size });

	while (sent < file.size) {
		signal?.throwIfAborted();
		const end = Math.min(sent + CHUNK_BYTES, file.size);
		const response = await fetch(absolute(base, handle.location), {
			method: 'PATCH',
			headers: {
				Authorization: `Bearer ${key}`,
				'Tus-Resumable': TUS_VERSION,
				'Upload-Offset': String(sent),
				'Content-Type': 'application/offset+octet-stream'
			},
			body: file.slice(sent, end),
			signal
		});

		if (response.status === 409) {
			// The server's offset moved under us — another tab, or a retry that landed after we gave up on
			// it. Re-reading rather than failing is the whole point of a resumable protocol.
			sent = await offsetOf(base, key, handle.location);
			onprogress({ sent, total: file.size });
			continue;
		}
		if (!response.ok) {
			throw new Error(`upload failed at ${sent} of ${file.size} bytes (${response.status})`);
		}

		// From the response, not from `end`: if the server accepted fewer bytes than were sent, the next
		// chunk has to start where it actually stopped.
		const reported = Number(response.headers.get('upload-offset'));
		sent = Number.isFinite(reported) && reported > sent ? reported : end;
		onprogress({ sent, total: file.size });
	}
}

/** A `Location` may be absolute or path-only; both are legal. */
function absolute(base: string, location: string): string {
	return location.startsWith('http') ? location : `${base}${location}`;
}
