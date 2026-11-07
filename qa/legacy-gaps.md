# Prompt 10 — Parità legacy: lacune e input necessari

**Stato: NON VERIFICATO.** Nessun accesso al YouWorkforce esistente in questa sessione.
La parità **non** è passata per assenza di dati.

## Input necessari per completare la matrice di parità
1. Accesso in sola lettura autorizzato al YouWorkforce esistente **oppure** export + manuali.
2. Elenco ruoli effettivi e permessi reali in uso.
3. API/export disponibili (formato, schema, volumi).
4. Integrazioni in uso (badge, centrale, ERP/presenze, notifiche).
5. Per cliente: anagrafiche, gruppi, sedi, assegnazioni, dispositivi, configurazioni, storico.

## Matrice (da compilare) — colonne
`funzione/entità | cliente/profilo | evidenza | mappatura nel nuovo prodotto | comportamento differente | stato{verificata|da_migrare|da_sostituire|da_decidere|non_verificata}`

## Migrazione (disegno)
- Per tenant, con staging e **dry-run** idempotente; riconciliazione conteggi/riferimenti; scarti con motivazione.
- Canale di allarme migrato **separatamente** dai dati: un solo sistema operativo per sito, finestra e responsabilità di passaggio, **rollback verificato**.
- Non disattivare il sistema esistente né passare dispositivi reali senza accordo e collaudo.
