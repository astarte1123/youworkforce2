# Autentika in YouWorkforce — analisi e piano di integrazione

Stato al 26/11/2026: **fase A1 (login) fatta, affiancata al login con password**; le altre fasi
sono da fare. Fonte: il repository `YouCo/autentika`, in
particolare `docs/integration/` (contratto consumatore, OIDC, BFF, licenze, ruoli, journal, scambio
token, logout) e le specifiche di provisioning `deploy/provision/specs/`.

## In breve

Autentika è l'Identity Provider multi-tenant di YouCo (Go, PostgreSQL). Parla **OpenID Connect
standard**: login con Authorization Code + PKCE, token di servizio `client_credentials`, scambio di
token RFC 8693, back-channel logout. Aggiunge due pezzi propri:

- un **giornale** (journal) firmato, da leggere a richiesta, per tenere allineati utenti, gruppi e ruoli;
- una **licenza per prodotto e per tenant**, letta come documento firmato (il «lease») e verificata
  offline con una PKI di YouCo.

Per YW significa tre cose:

- il **login** passa ad Autentika: YW resta un BFF con la propria sessione;
- gli **utenti** sono chiave `(tenant_id, sub)` di Autentika;
- le **licenze** di oggi del superadmin vengono dal lease del prodotto `youworkforce`.

Il prodotto `youworkforce` è già previsto nel catalogo (ADR-034) ma non è ancora definito.

## Cosa c'è già di YouWorkforce in Autentika

- Nei tenant `youco` e `youco-dev` esiste il client **`youworkforce`**: confidenziale,
  `client_credentials`, credenziale del principale di servizio `youworker-backend`, concesso sulla
  risorsa `youworker-api`.
- C'è un **broker grant** `urn:youco:channel:youworkforce` → `youworker-api`. Con la sua credenziale
  YW può ottenere un token **per conto di un utente**: il `sub` è l'utente, `act.sub` è
  `youworker-backend`. Lo identifica l'UUID dell'utente di Autentika. È il canale con cui YW chiama
  l'AI di YouWorker «come quell'utente» (fase F5).

Quindi il canale YW → YouWorker esiste già lato Autentika. Manca tutto il resto: login, utenti,
licenze di YW.

## Come funziona Autentika, per quanto ci riguarda

| Aspetto | Autentika | Conseguenza per YW |
|---|---|---|
| Tenant | **tenant = issuer** (`https://<slug>.autentika.youco.cloud` o dominio proprio), chiavi ES256 e JWKS propri, isolamento RLS | Ogni azienda cliente di YW (Idealverde, …) è un tenant di Autentika. YW memorizza per ogni suo tenant l'issuer e il `tenant_id`. |
| Client | **un client per tenant** (un `client_id` vale solo nel suo tenant); niente registrazione dinamica | YW ha un client `youworkforce-web` in **ogni** tenant cliente, con il suo segreto, da tenere cifrato. Va creato con `autentika provision` (specifiche come quelle di YouWorker). |
| Login | Authorization Code con PKCE S256 obbligatorio; Autentika non mostra pagine: autentica tramite un IdP del tenant (Google, Entra, Okta) o la pagina di login del client (`login_uri`) | YW fa da BFF: tiene il cookie `yw_session` e i token lato server. Il browser non vede mai token di Autentika. |
| Token | access token ES256 di 5 minuti, `aud` = **una** risorsa, `roles`/`permissions` del prodotto di quella risorsa, `groups` con lo scope; refresh che ruota ogni volta (30 giorni); sessione 24 h assolute, 30 minuti di inattività | YW verifica con la JWKS del tenant e rinnova almeno ogni 30 minuti finché la sessione è attiva. |
| Utenti | `sub` = `user_<uuid>`, stabile (non cambia con email, sospensione, ruoli); mai la chiave per email | Su `app.users`: `auth_tenant_id` + `auth_sub` unici; l'email resta un attributo. |
| Ruoli | definiti **dal prodotto**, uguali per tutti i tenant; assegnati a persone, gruppi o «Members» (tutti); il **relation gate** non dà token a chi non ha ruoli del prodotto | Chi non ha un ruolo `youworkforce` non entra. La granularità fine (capability, ruoli personalizzati, sedi) può restare in YW. |
| Licenze | una per (prodotto, tenant): stato `active`/`grace`/`expired`/`revoked`, `terms` JSON (Autentika interpreta solo `seats`), lease firmato dalla PKI offline, verificabile senza rete fino a `max_offline` | Sostituisce `mobile_app_licenses`/`iot_device_licenses` e l'attivazione dei moduli del superadmin: i `terms` portano posti, moduli e limiti. |
| Allineamento | **journal** a richiesta (sospensioni, gruppi, ruoli, licenze) con un token di servizio con `journal:read`; **back-channel logout** sulle sessioni | Un utente sospeso o tolto in Autentika si disattiva in YW entro un minuto; un logout altrove chiude la sessione YW. |
| Servizi | principali di servizio con `client_credentials`; token `aud` = risorsa del prodotto | Le chiavi di integrazione `ywk_…` (gestionale, Youco) possono restare, oppure diventare principali di servizio di Autentika (fase successiva). |
| Password | Argon2id; **gli hash esistenti non si importano**: chi migra imposta una nuova password o entra con l'IdP | Gli utenti di oggi vanno invitati di nuovo: fa parte della migrazione. |

## Architettura proposta

```
Browser / app lavoratore           YW core (BFF, Rust)                      Autentika (issuer del tenant)
  cookie yw_session      ──►  /api/auth/oidc/start  ── code + PKCE ──────►  /oauth2/auth (IdP o pagina YW)
                         ◄──  /api/auth/oidc/callback ◄── code ───────────  
                              token lato server, sessione YW
                              /api/auth/backchannel-logout  ◄── logout_token ──
                              worker: journal ogni minuto  ── service token ─►  /v1/journal
                              worker: lease ogni ora       ── client secret ─►  /v1/license/lease?product=youworkforce
                              agente AI: broker exchange   ── youworkforce ──►  /oauth2/token (→ youworker-api)
```

- **Scoperta del tenant al login.** Si usa lo slug che YW ha già (`x-tenant-slug`, sottodominio):
  ogni tenant YW ha `auth_issuer` e il client in Autentika. YW legge la discovery dell'issuer e
  avvia il flusso. Un tenant senza Autentika resta con il login locale per tutta la transizione.
- **Sessione.** Dopo la callback YW crea la sua sessione come oggi (cookie `HttpOnly`, CSRF) e la
  lega a `{sub, tenant_id, sid, refresh_token}`. La validità resta in YW: se Autentika non risponde,
  le sessioni aperte continuano e gli allarmi non si fermano. Si bloccano solo i nuovi login e i
  rinnovi.
- **Primo accesso (JIT).** Un `sub` sconosciuto con un ruolo `youworkforce` valido crea l'utente YW
  con il ruolo che arriva dal token. Gli inviti di YW diventano inviti di Autentika (bypassano la
  registrazione chiusa).
- **Ruoli (proposta).** Prodotto `youworkforce` con i **ruoli base di YW**: `admin`, `pianificatore`,
  `responsabile`, `lavoratore`, `reperibile`, `operatore`, `auditor`.
  - Si assegnano in Autentika, a persone o gruppi.
  - Restano in YW, legati a `(tenant_id, sub)`: capability, ruoli personalizzati, sedi consentite e
    gruppi operativi (squadre).
  - Così Autentika decide **chi entra e con che ruolo**, YW decide **cosa fa nel dettaglio**.
- **Licenza (proposta).** Prodotto `youworkforce` con `enforcement_mode: self`: YW legge il lease e
  applica le regole. I `terms` contengono:
  - `seats`: utenti, contati da Autentika;
  - `modules`: i moduli attivi (oggi li sceglie il superadmin);
  - `mobile_app`, `iot_devices`: limiti di oggi.

  Con licenza scaduta: login bloccato e sola lettura per gli amministratori. **Sicurezza dei
  lavoratori e allarmi mai bloccati**, come la regola salva-vita di oggi. Il lease si verifica
  offline: root `afd6d572…714e` dentro la release, controllo della CRL, `exp`.
- **Superadmin.** Per l'avvio resta il login locale con MFA, come accesso di emergenza. Dopo può
  passare al tenant `platform` di Autentika (`autentika/platform-owner`) o a un tenant YouCo dedicato.
- **App del lavoratore (PWA).** Stesso flusso BFF. Per i turni lunghi serve una sessione che non
  scada a 30 minuti di inattività: refresh periodico dall'app, oppure famiglia `offline_access`
  (dura finché non viene revocata). Da decidere con YouCo.

## Librerie (Rust, nessuna nuova dipendenza esterna obbligatoria)

- **`ring` 0.17**, già presente, verifica le firme ES256 di id_token, access token, logout token,
  pagine del journal e lease.
- Per il JWT basta un piccolo modulo: decodifica base64url, controllo di `kid`, `typ`, `alg`, `iss`,
  `aud` ed `exp`, cache della JWKS con un nuovo tentativo se manca il `kid`.
- **`rustls-webpki` 0.103**, già presente, può verificare la catena `x5c` del lease fino alla root:
  da provare con l'uso delle chiavi della PKI di licenza.
- **`reqwest`** per le chiamate.
- In alternativa la crate `openidconnect`, che però va aggiunta al deposito offline.

## Piano a fasi

| Fase | Contenuto | Verifica |
|---|---|---|
| **A0** Prerequisiti con YouCo | prodotto `youworkforce` (risorse, ruoli, `lease_audiences`), tenant di sviluppo, client `youworkforce-web` con redirect/`error_uri`/`login_uri`/`backchannel_logout_uri`, decisione sulle domande sotto | discovery e JWKS del tenant raggiungibili; client e segreto consegnati |
| **A1** Login OIDC (BFF) | `auth_issuer`/client cifrato per tenant, `/api/auth/oidc/start` e `/callback` con PKCE, verifica id_token, `users.auth_sub`, JIT, sessione YW legata a `sid`, refresh, logout RP, back-channel logout; login locale ancora disponibile per tenant | e2e contro un Autentika locale (docker compose del repo): login, ruolo assente → negato, logout altrove → sessione YW chiusa |
| **A2** Allineamento utenti | worker che legge il journal (snapshot + coda) con token di servizio `journal:read`: sospensione, eliminazione, ruoli, gruppi → utenti YW; console: utenti «gestiti da Autentika» non modificabili nei campi di identità | sospensione in Autentika → utente YW disattivato entro 1 minuto |
| **A3** Licenze | lettura del lease ogni ora e a ogni fatto `license`, verifica offline (root, CRL, catena), `terms` → moduli/posti/limiti, pagina superadmin in sola lettura con stato e scadenza | licenza in `grace` → avviso; `revoked` → login bloccato, allarmi attivi |
| **A4** Migrazione | per tenant: creazione utenti in Autentika (invito o IdP aziendale), collegamento `sub` ↔ utente YW esistente per email verificata una sola volta, poi solo `sub` | Idealverde migrato senza perdere storico né assegnazioni |
| **A5** Servizi e AI | broker exchange `youworkforce` per le chiamate all'AI di YouWorker come l'utente; facoltativo: chiavi di integrazione come principali di servizio Autentika | l'agente AI chiama `youworker-api` con `act.sub = youworker-backend` |

## Fase A1 — fatta: «Login con Autentika» accanto al login con password

Migrazione `0172`, codice `core/src/autentika.rs`, test `qa/autentika-login-e2e.sh`. Il test gira
contro un finto Autentika locale, `qa/oidc_mock.py`, con firme ES256. **Entrambi i login restano
attivi.**

- **Configurazione** (superadmin → tenant → «Login con Autentika»):
  - issuer del tenant Autentika, client ID, segreto (cifrato con la chiave dei segreti), indirizzo di
    ritorno;
  - scope, provider preferito (`idp_hint`), risorsa per i ruoli (facoltativa);
  - attivo, crea l'utente al primo accesso, collega per email verificata;
  - ruolo predefinito e corrispondenza dei ruoli.

  **Verifica collegamento** controlla discovery, JWKS, PKCE S256 e credenziali del client: un codice
  inventato deve dare `invalid_grant`, non `invalid_client`.
- **Pagina di accesso:** sotto «Accedi» compare **«Login con Autentika»** se il tenant lo ha attivo
  (`GET /api/auth/providers`). Al ritorno con un errore, il messaggio appare sotto il modulo.
- **Flusso:**
  1. `GET /api/auth/oidc/start` genera state, nonce e PKCE S256, li tiene sul server per 10 minuti
     (`app.oidc_pending`) e porta all'`authorization_endpoint` dell'issuer.
  2. Autentika torna su `GET /api/auth/oidc/callback`, che scambia il codice (`client_secret_basic`)
     e verifica l'id_token: ES256 con `ring`, `kid` dalla JWKS con nuova lettura se manca, niente
     `jku`/`x5u`/`x5c`, `iss`, `aud`, `exp`, `nonce`.
  3. Con la risorsa configurata, i `roles` arrivano dall'access token (`aud` = risorsa).
  4. Si crea la sessione YW (stesso cookie del login con password), legata al `sid` di Autentika.
- **Utente:** chiave `(tenant, sub)` in `users.auth_sub`. Al primo accesso:
  - si collega l'utente YW con la stessa email **solo se verificata**, e il ruolo YW non cambia;
  - altrimenti si crea l'utente con il ruolo più alto tra quelli mappati, o con quello predefinito.

  L'utente creato non ha password locale.
- **Logout:**
  - `POST /api/auth/backchannel-logout`: un `logout_token` firmato, con l'evento di logout e il
    client giusto, chiude le sessioni YW di quel `sid`;
  - il logout da YW chiude anche la sessione di Autentika (`end_session_endpoint` con
    `id_token_hint`, best effort).

### Provarlo con un Autentika vero

1. In Autentika (tenant di prova, es. `youco-dev`, o l'Autentika locale del repo) registrare un
   client:
   - `application_type: web`, `client_secret_basic`;
   - `grant_types: [authorization_code, refresh_token]`;
   - `scope: openid profile email groups`.
2. Indirizzi del client:
   - `redirect_uris`: `https://<slug>.<dominio-yw>/api/auth/oidc/callback`, identico a quello del
     superadmin;
   - `error_uri`: `https://<slug>.<dominio-yw>/`;
   - `backchannel_logout_uri`: `https://<slug>.<dominio-yw>/api/auth/backchannel-logout`.
3. Ruoli: per averli, una risorsa del prodotto concessa al client e i ruoli assegnati agli utenti.
   Senza risorsa si usa il ruolo predefinito.
4. Nel superadmin di YW: issuer (`https://youco-dev.autentika.youco.cloud`), client e segreto,
   **Verifica collegamento**, **Attivo**.
5. Il redirect deve essere `https` e raggiungibile dal browser. `http` è ammesso solo con un issuer
   locale (`http://127.0.0.1…`), come nei test.

**Limiti di questa fase:**

- la sessione di Autentika non viene rinnovata (niente refresh): la sessione YW dura le sue 12 ore;
- il ruolo si prende da Autentika solo alla creazione dell'utente, poi lo sincronizza la fase A2 con
  il journal;
- le licenze restano quelle del superadmin fino alla fase A3.

## Domande aperte per YouCo

1. **Tenant.** Ogni azienda cliente di YW è un tenant di Autentika (consigliato: licenza, IdP e
   isolamento sono per tenant)? Oppure i clienti stanno in un tenant comune?
2. **Lavoratori senza Google.** I giardinieri spesso non hanno un account aziendale. Con issuer e app
   su domini diversi, il login con password di Autentika richiede la pagina di login sullo stesso
   dominio padre dell'issuer: dominio personalizzato tipo `auth.<dominio-yw>`. Come preferiscono
   farlo entrare: password su dominio dedicato, passkey, IdP del cliente?
3. **Prodotto `youworkforce`.** Chi definisce risorse e ruoli nel catalogo? Va bene la mappa sui
   ruoli base di YW? I ruoli personalizzati restano in YW?
4. **Licenza.** Modalità `self` (YW applica) o `gate` (Autentika nega i token)? Quali chiavi nei
   `terms` (moduli, posti, dispositivi IoT, app)? Quanto `max_offline`?
5. **Canale AI.** Il broker `urn:youco:channel:youworkforce` vale solo per `youco`/`youco-dev`.
   Per i tenant cliente servono broker grant analoghi?
6. **Superadmin di YW.** Resta locale con MFA o passa ad Autentika (tenant `platform` o tenant YouCo)?
7. **Ambienti.** Possiamo avere un tenant di sviluppo (es. `youco-dev`) con il client di YW per i test
   end-to-end? Oppure usiamo l'Autentika locale del repo?

## Cosa non cambia

- Sicurezza del lavoratore, allarmi e dispositivi non dipendono dalla disponibilità di Autentika.
  I dispositivi usano le loro credenziali; le sessioni YW restano valide.
- Isolamento per tenant in YW (RLS su `app.current_tenant`), capability e ACL delle sedi.
- Chiavi di integrazione `ywk_…` finché non si decide la fase A5.
