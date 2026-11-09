/**
 * New passkey backups are off until the passkey domain is set (audit H-1):
 * switching the domain breaks passkeys made before it. Restoring with an
 * existing passkey still works.
 */
export const PASSKEY_BACKUPS = false;
