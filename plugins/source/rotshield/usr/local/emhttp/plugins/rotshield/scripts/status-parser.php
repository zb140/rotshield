<?php

// Parse scanner status blocks and runner metadata without loading
// the entire log into memory.
function parse_scrub_log($path) {
    $blocks = [];
    $outcome = "";
    $active = "";

    $fp = fopen($path, "r");
    if (!$fp) {
        return null;
    }

    $cur = null;

    while (($line = fgets($fp)) !== false) {
        $line = rtrim($line);

        if ($line === "status:") {
            if ($cur !== null) {
                $blocks[] = $cur;
            }
            $cur = [];
            continue;
        }

        if ($cur !== null) {
            if (preg_match('/^([a-z_][a-z0-9_]*)=(.*)$/', $line, $m)) {
                $cur[$m[1]] = $m[2];
                continue;
            }

            $blocks[] = $cur;
            $cur = null;
        }

        if (preg_match('/finished:\s*(.+)$/', $line, $m)) {
            $outcome = $m[1];
        }

        if (preg_match('/scrubbing\s+(\/\S+)/', $line, $m)) {
            $active = $m[1];
        }
    }

    if ($cur !== null) {
        $blocks[] = $cur;
    }

    fclose($fp);

    return [
        'blocks' => $blocks,
        'outcome' => $outcome,
        'active' => $active,
    ];
}

?>
