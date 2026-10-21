<?php

declare(strict_types=1);

namespace Stubbedev\DsMcp;

final class Binary
{
    private const NAME = 'ds-mcp';
    private const REPO = 'stubbedev/' . self::NAME;

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

    public static function ensure(): string
    {
        $dest = self::root() . '/bin/' . self::NAME . '-native' . self::exe();
        if (self::present($dest)) {
            return $dest;
        }
        $cached = self::cacheDir() . '/' . self::NAME . '_' . self::target() . self::exe();
        if (!self::present($cached)) {
            self::fetch($cached);
        }
        self::place($cached, $dest);

        return $dest;
    }

    private static function root(): string
    {
        return dirname(__DIR__, 3);
    }

    private static function windows(): bool
    {
        return PHP_OS_FAMILY === 'Windows';
    }

    private static function exe(): string
    {
        return self::windows() ? '.exe' : '';
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

    private static function version(): string
    {
        $file = self::root() . '/Cargo.toml';
        $toml = (string) @file_get_contents($file);
        if (!preg_match('/^version\s*=\s*"([^"]+)"/m', $toml, $m)) {
            throw new \RuntimeException("Cannot read the version from {$file}");
        }

        return $m[1];
    }

    private static function fetch(string $cached): void
    {
        $tag = 'v' . self::version();
        $archive = sprintf('%s_%s_%s%s', self::NAME, $tag, self::target(), self::windows() ? '.zip' : '.tar.gz');
        $base = sprintf('https://github.com/%s/releases/download/%s', self::REPO, $tag);

        $tmp = self::tmpBeside($cached);
        if (!@mkdir($tmp, 0755)) {
            throw new \RuntimeException("Cannot create {$tmp}: " . self::lastError());
        }
        try {
            self::download("{$base}/checksums.txt", "{$tmp}/checksums.txt");
            $sums = (string) file_get_contents("{$tmp}/checksums.txt");
            if (!preg_match('/^([0-9a-f]{64})\s+\*?' . preg_quote($archive, '/') . '$/m', $sums, $m)) {
                throw new \RuntimeException("{$archive} is not listed in {$tag} checksums.txt");
            }
            self::download("{$base}/{$archive}", "{$tmp}/{$archive}");
            if (!hash_equals($m[1], hash_file('sha256', "{$tmp}/{$archive}"))) {
                throw new \RuntimeException("Checksum mismatch for {$archive}");
            }
            $exe = self::NAME . self::exe();
            if (self::windows()) {
                $zip = new \ZipArchive();
                if ($zip->open("{$tmp}/{$archive}") !== true || !$zip->extractTo($tmp, $exe)) {
                    throw new \RuntimeException("Cannot unpack {$archive}");
                }
                $zip->close();
            } else {
                (new \PharData("{$tmp}/{$archive}"))->extractTo($tmp, $exe, true);
            }
            self::commit("{$tmp}/{$exe}", $cached);
        } finally {
            array_map('unlink', glob("{$tmp}/*") ?: []);
            @rmdir($tmp);
        }
    }

    private static function cacheDir(): string
    {
        return self::cacheRoot() . '/' . self::NAME . '/' . self::version();
    }

    private static function cacheRoot(): string
    {
        $xdg = getenv('XDG_CACHE_HOME');
        if (is_string($xdg) && $xdg !== '') {
            return $xdg;
        }
        [$var, $suffix] = match (PHP_OS_FAMILY) {
            'Windows' => ['LOCALAPPDATA', ''],
            'Darwin' => ['HOME', '/Library/Caches'],
            default => ['HOME', '/.cache'],
        };
        $base = getenv($var);
        if (!is_string($base) || $base === '') {
            throw new \RuntimeException("Cannot locate a cache directory: neither XDG_CACHE_HOME nor {$var} is set");
        }

        return $base . $suffix;
    }

    private static function present(string $file): bool
    {
        return is_file($file) && filesize($file) > 0;
    }

    private static function place(string $cached, string $dest): void
    {
        $tmp = self::tmpBeside($dest);
        try {
            if (!@link($cached, $tmp) && !@copy($cached, $tmp)) {
                throw new \RuntimeException("Cannot link or copy {$cached} to {$tmp}: " . self::lastError());
            }
            self::commit($tmp, $dest);
        } finally {
            @unlink($tmp);
        }
    }

    private static function commit(string $tmp, string $dest): void
    {
        if (!@chmod($tmp, 0755) || !@rename($tmp, $dest)) {
            throw new \RuntimeException("Cannot move {$tmp} into place at {$dest}: " . self::lastError());
        }
    }

    private static function tmpBeside(string $file): string
    {
        $dir = dirname($file);
        if (!is_dir($dir) && !@mkdir($dir, 0755, true) && !is_dir($dir)) {
            throw new \RuntimeException("Cannot create {$dir}: " . self::lastError());
        }

        return "{$file}." . bin2hex(random_bytes(6)) . '.tmp';
    }

    private static function download(string $url, string $file): void
    {
        $out = @fopen($file, 'wb');
        if ($out === false) {
            throw new \RuntimeException("Cannot write {$file}: " . self::lastError());
        }
        try {
            if (function_exists('curl_init')) {
                self::curl($url, $out);
            } else {
                self::stream($url, $out);
            }
        } finally {
            fclose($out);
        }
        clearstatcache(true, $file);
        if (filesize($file) === 0) {
            throw new \RuntimeException("Failed to download {$url}: empty response");
        }
    }

    /** @param resource $out */
    private static function curl(string $url, $out): void
    {
        $ch = curl_init($url);
        curl_setopt_array($ch, [
            CURLOPT_FILE => $out,
            CURLOPT_FOLLOWLOCATION => true,
            CURLOPT_FAILONERROR => true,
            CURLOPT_USERAGENT => self::NAME . '-composer',
            CURLOPT_CONNECTTIMEOUT => 30,
            CURLOPT_LOW_SPEED_LIMIT => 1024,
            CURLOPT_LOW_SPEED_TIME => 30,
        ]);
        if (curl_exec($ch) === false) {
            throw new \RuntimeException("Failed to download {$url}: " . curl_error($ch));
        }
    }

    /** @param resource $out */
    private static function stream(string $url, $out): void
    {
        $ctx = stream_context_create(['http' => [
            'follow_location' => 1,
            'user_agent' => self::NAME . '-composer',
            'timeout' => 30,
        ]]);
        $in = @fopen($url, 'rb', false, $ctx);
        if ($in === false) {
            throw new \RuntimeException("Failed to download {$url}: " . self::lastError());
        }
        try {
            if (stream_copy_to_stream($in, $out) === false) {
                throw new \RuntimeException("Failed to download {$url}: " . self::lastError());
            }
        } finally {
            fclose($in);
        }
    }

    private static function lastError(): string
    {
        return error_get_last()['message'] ?? 'unknown error';
    }
}
