/*
 * The frozen declarations of the tickertape C ABI (ADR 0028 -> Versioning), checked by
 * .github/scripts/check_abi_baseline.py against include/tickertape.h in CI.
 *
 * Nothing is frozen before tickertape 1.0, so this file declares nothing. When 1.0 is released, replace it with that
 * release's include/tickertape.h (docs/playbooks/release-checklist.md). From then on, removing or changing a
 * declaration in it fails CI; adding declarations to the header does not.
 */
