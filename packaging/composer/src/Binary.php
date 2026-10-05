<?php

declare(strict_types=1);

namespace Stubbedev\DsMcp;

// Resolves and downloads the prebuilt binary for this OS/arch from the GitHub
// release matching Cargo.toml's version (the same targets, archives and
// checksum check as scripts/download.mjs). Shared by the Composer plugin,
// which fetches it at install time, and the ds-mcp launcher, which fetches it
// on first run if the plugin was not allowed to.
final class Binary
{
    public const REPO = 'stubbedev/ds-mcp';

    // Release targets built by .github/workflows/release.yml.
    private const TARGETS = [
        'Linux' => ['x86_64' => 'x86_64-unknown-linux-gnu', 'aarch64' => 'aarch64-unknown-linux-gnu'],
        'Darwin' => ['x86_64' => 'x86_64-apple-darwin', 'aarch64' => 'aarch64-apple-darwin'],
        'Windows' => ['x86_64' => 'x86_64-pc-windows-msvc'],
    ];

    private const ARCH = [
        'x86_64' => 'x86_64',
        'amd64' => 'x86_64',
        'aarch64' => 'aarch64',
        'arm64' => 'aarch64',
    ];

    private static function root(): string
    {
        return dirname(__DIR__, 3);
    }

    private static function windows(): bool
    {
        return PHP_OS_FAMILY === 'Windows';
    }

    // Cached inside the package dir (gitignored, like the npm wrapper's copy).
    public static function path(): string
    {
        return self::root() . '/bin/ds-mcp-native' . (self::windows() ? '.exe' : '');
    }

    // Returns the path to the platform binary, downloading it if not present.
    public static function ensure(): string
    {
        $dest = self::path();
        if (is_file($dest) && filesize($dest) > 0) {
            return $dest;
        }
        self::install($dest);

        return $dest;
    }

    private static function target(): string
    {
        $machine = strtolower(php_uname('m'));
        $target = self::TARGETS[PHP_OS_FAMILY][self::ARCH[$machine] ?? ''] ?? null;
        if ($target === null) {
            throw new \RuntimeException(sprintf(
                'Unsupported platform %s/%s. Build from source (https://github.com/%s#install) and set DS_MCP_BINARY.',
                PHP_OS_FAMILY,
                $machine,
                self::REPO,
            ));
        }

        return $target;
    }

    // Cargo.toml is the single source of truth for the version, and the
    // release tag is cut from it, so a pinned package gets that tag's binary.
    private static function version(): string
    {
        $file = self::root() . '/Cargo.toml';
        $toml = (string) @file_get_contents($file);
        if (!preg_match('/^version\s*=\s*"([^"]+)"/m', $toml, $m)) {
            throw new \RuntimeException("Cannot read the version from {$file}");
        }

        return $m[1];
    }

    private static function install(string $dest): void
    {
        $tag = 'v' . self::version();
        $archive = sprintf('ds-mcp_%s_%s%s', $tag, self::target(), self::windows() ? '.zip' : '.tar.gz');
        $base = sprintf('https://github.com/%s/releases/download/%s', self::REPO, $tag);

        $sums = self::fetch("{$base}/checksums.txt");
        if (!preg_match('/^([0-9a-f]{64})\s+\*?' . preg_quote($archive, '/') . '$/m', $sums, $m)) {
            throw new \RuntimeException("{$archive} is not listed in {$tag} checksums.txt");
        }
        $data = self::fetch("{$base}/{$archive}");
        if (!hash_equals($m[1], hash('sha256', $data))) {
            throw new \RuntimeException("Checksum mismatch for {$archive}");
        }

        $dir = dirname($dest);
        if (!is_dir($dir) && !@mkdir($dir, 0755, true) && !is_dir($dir)) {
            throw new \RuntimeException("Cannot create {$dir}");
        }
        $tmp = "{$dir}/.download-" . bin2hex(random_bytes(6));
        mkdir($tmp);
        try {
            file_put_contents("{$tmp}/{$archive}", $data);
            $exe = self::windows() ? 'ds-mcp.exe' : 'ds-mcp';
            if (self::windows()) {
                $zip = new \ZipArchive();
                if ($zip->open("{$tmp}/{$archive}") !== true || !$zip->extractTo($tmp, $exe)) {
                    throw new \RuntimeException("Cannot unpack {$archive}");
                }
                $zip->close();
            } else {
                (new \PharData("{$tmp}/{$archive}"))->extractTo($tmp, $exe, true);
            }
            @chmod("{$tmp}/{$exe}", 0755);
            @unlink($dest);
            // Rename last so a concurrent first run never sees a partial binary.
            if (!rename("{$tmp}/{$exe}", $dest)) {
                throw new \RuntimeException("Cannot move the binary into place at {$dest}");
            }
        } finally {
            array_map('unlink', glob("{$tmp}/*") ?: []);
            @rmdir($tmp);
        }
    }

    private static function fetch(string $url): string
    {
        if (function_exists('curl_init')) {
            $ch = curl_init($url);
            curl_setopt_array($ch, [
                CURLOPT_RETURNTRANSFER => true,
                CURLOPT_FOLLOWLOCATION => true,
                CURLOPT_FAILONERROR => true,
                CURLOPT_USERAGENT => 'ds-mcp-composer',
                CURLOPT_CONNECTTIMEOUT => 30,
            ]);
            $body = curl_exec($ch);
            $err = curl_error($ch);
            // No curl_close(): a no-op since PHP 8.0, deprecated as of 8.5.
            unset($ch);
        } else {
            $ctx = stream_context_create(['http' => [
                'follow_location' => 1,
                'user_agent' => 'ds-mcp-composer',
            ]]);
            $body = @file_get_contents($url, false, $ctx);
            $err = error_get_last()['message'] ?? 'unknown error';
        }
        if (!is_string($body) || $body === '') {
            throw new \RuntimeException("Failed to download {$url}: {$err}");
        }

        return $body;
    }
}
