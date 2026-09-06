#!/bin/bash
set -euo pipefail

if (($# != 1)); then
    echo "usage: $0 KERNEL_TREE" >&2
    exit 2
fi

kernel_tree=$(realpath -e -- "$1")
driver="$kernel_tree/drivers/gpu/drm/adp/adp_drv.c"
panel="$kernel_tree/drivers/gpu/drm/panel/panel-summit.c"

for source in "$driver" "$panel"; do
    if [[ ! -f "$source" || -L "$source" ]]; then
        echo "required regular kernel source is missing: $source" >&2
        exit 1
    fi
done

rg -q '^#define ADP_INT_STATUS_VBLANK 0x1$' "$driver"
rg -q 'platform_get_irq_byname\(pdev, "be"\)' "$driver"
rg -q 'platform_get_irq_byname\(pdev, "fe"\)' "$driver"
rg -q 'request_irq\(adp->fe_irq, adp_fe_irq' "$driver"
if rg -q 'request_irq\(adp->be_irq' "$driver"; then
    echo "ADP backend IRQ is now requested; repeat the cadence investigation" >&2
    exit 1
fi
rg -q 'drm_crtc_handle_vblank\(&adp->crtc\)' "$driver"
rg -q 'FIXME: use adbe flush interrupt' "$driver"
if rg -q 'mode_set_nofb|atomic_mode_set|\.mode_set[[:space:]]*=' "$driver"; then
    echo "ADP now has a timing/mode-setting path; repeat the cadence investigation" >&2
    exit 1
fi
rg -Uq '\.clock = \(\(60 \+ 8 \+ 80 \+ 40\).*\* 60\) / 1000' "$panel"

revision=unknown
if git -C "$kernel_tree" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    revision=$(git -C "$kernel_tree" rev-parse --short=12 HEAD)
fi

echo "adp-source-audit=ok revision=$revision advertised_hz=60 vblank_source=fe backend_irq=unused timing_programming=absent"
