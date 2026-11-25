<?php

require_once __DIR__ . '/status-parser.php';

// Read the persistent results database. Missing is normal;
// malformed or unsupported data must not be silently overwritten.
function load_results($path) {
    if (!file_exists($path)) {
        return (object) [
            'schema_version' => 1,
            'disks' => new stdClass(),
        ];
    }

    $contents = file_get_contents($path);
    if ($contents === false) {
        throw new RuntimeException("Cannot read results file");
    }

    $results = json_decode($contents, false, 512, JSON_THROW_ON_ERROR);

    if (($results->schema_version ?? null) !== 1 ||
        !is_object($results->disks ?? null)) {
        throw new RuntimeException("Invalid or unsupported results schema");
    }

    return $results;
}

// Replace the database atomically, preserving the old file
// if serialization or writing fails.
function save_results($path, $results) {
    $json = json_encode(
        $results,
        JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES | JSON_THROW_ON_ERROR
    ) . "\n";

    $tmp = tempnam(dirname($path), '.results-');
    if ($tmp === false) {
        throw new RuntimeException("Cannot create temporary results file");
    }

    try {
        if (file_put_contents($tmp, $json) !== strlen($json)) {
            throw new RuntimeException("Cannot write results file");
        }

        if (!rename($tmp, $path)) {
            throw new RuntimeException("Cannot replace results file");
        }
    } finally {
        if (file_exists($tmp)) {
            unlink($tmp);
        }
    }
}

// Update one disk without discarding other disks' results.
// Caller must hold the runner's synchronization lock.
function record_result($path, $disk_id, $record) {
    if ($disk_id === '') {
        throw new InvalidArgumentException("Missing disk identity");
    }

    $results = load_results($path);
    $results->disks->{$disk_id} = $record;
    save_results($path, $results);
}

// Extract the last completed status block for the expected device.
// Missing counters remain absent rather than defaulting to zero.
function extract_counters($log_path, $device, $state = 'done') {
    $parsed = parse_scrub_log($log_path);
    if ($parsed === null) {
        throw new RuntimeException("Cannot read scanner log");
    }

    $counters = null;

    foreach ($parsed['blocks'] as $block) {
        if (($block['device'] ?? '') !== $device ||
            ($block['state'] ?? '') !== $state) {
            continue;
        }

        $counters = [];

        foreach ($block as $key => $value) {
            if (in_array($key, ['state', 'device'], true)) {
                continue;
            }

            if (preg_match('/^[0-9]+$/', $value)) {
                $counters[$key] = (int) $value;
            } elseif (is_numeric($value)) {
                $counters[$key] = (float) $value;
            }
        }
    }

    return $counters;
}

// CLI entry point used by the scrub runner.
if (PHP_SAPI === 'cli' &&
    realpath($_SERVER['SCRIPT_FILENAME'] ?? '') === __FILE__) {
    try {
        if ($argc !== 10) {
            throw new InvalidArgumentException(
                "Usage: results.php <results-file> <disk-id> " .
                "<device> <slot> <started-at> <outcome> " .
                "<exit-code> <repair-enabled> <scanner-log>"
            );
        }

        [
            , $path, $disk_id, $device, $slot,
            $started_at, $outcome, $exit_code,
            $repair_enabled, $log_path
        ] = $argv;

        $record = [
            'device' => $device,
            'array_slot' => (int) $slot,
            'started_at' => $started_at,
            'finished_at' => gmdate('Y-m-d\TH:i:s\Z'),
            'outcome' => $outcome,
            'exit_code' => (int) $exit_code,
            'repair_enabled' => $repair_enabled === '1',
            'counters' => extract_counters(
              $log_path,
              $device,
              in_array($outcome, ['cancelled', 'safety_abort'], true)
              ? 'cancelled'
              : 'done'
            ),
        ];

        record_result($path, $disk_id, $record);
    } catch (Throwable $e) {
        fwrite(STDERR, "RotShield results: " . $e->getMessage() . "\n");
        exit(1);
    }
}

?>
