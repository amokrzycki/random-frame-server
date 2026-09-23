# Random Frame Sync — jak działa wdrożona usługa

Stan sprawdzony 24 września 2026 r. w kodzie obu repozytoriów i odczytowo na VPS `150.230.159.236`. To opis **obecnego wdrożenia**, nie plan nowej architektury. Statusy usług, liczba rekordów, adresy Cloudflare i ważność certyfikatu mogą się później zmienić. Nie ma tu recovery key, bearer tokenów ani danych pozwalających przejąć Sync.

## Najkrótszy obraz całości

```text
Random Frame na urządzeniu A/B
  ├─ lokalny SeenStore: prntsc-seen.json
  ├─ sekret główny: systemowy magazyn poświadczeń
  └─ konfiguracja parowania: sync-config.json
           │ HTTPS: zaszyfrowany envelope + bearer + rewizja
           ▼
Cloudflare (proxied DNS) → nginx na Oracle VPS:443
                            │ HTTP tylko po loopback
                            ▼
                    Axum 127.0.0.1:8787
                            │
                            ▼
                SQLite /var/lib/random-frame-sync/sync.db
                            │ poprawny backup aktywnej bazy
                            ▼
              /var/backups/random-frame-sync/
```

Klient jest źródłem prawdy o tym, **które identyfikatory klatek zostały obejrzane**. Serwer nie zna plaintextu, nie scala zbiorów i nie ma klucza odszyfrowującego. Zapewnia przechowanie zaszyfrowanych bajtów, kontrolę bearer tokenu i atomowe wersjonowanie. Sync jest dostępny w kliencie na Linux i Windows; pozostałe platformy nie są objęte obecną implementacją Sync.

## Co jest synchronizowane, a co nie

Synchronizowany jest zbiór numerycznych identyfikatorów obejrzanych klatek Prnt.sc (`SeenStore`). Lokalnie leży w katalogu danych aplikacji Tauri jako `prntsc-seen.json`. Migrowane na starcie wpisy ze starszej historii/eksploracji mogą ten zbiór uzupełnić. To nie jest synchronizacja obrazów, historii wyświetlania, ulubionych ani ustawień całej aplikacji.

`SeenStore` dodaje identyfikatory i zapisuje wynik lokalnie. Scalanie to suma zbiorów: jeśli A zna `{101, 303}`, a B `{202, 404}`, po udanym Sync oba mogą znać `{101, 202, 303, 404}`. Usunięcie ID z jednego urządzenia nie jest mechanizmem usuwania go z pozostałych. Aplikacja działa lokalnie również przy niedostępnym serwerze; nowe `seen` czekają na kolejny Sync.

Aktualnie Sync uruchamia się przy starcie sparowanej aplikacji oraz przez **Sync now**. Nie ma w tym wdrożeniu stale działającego procesu synchronizacji w tle. Ikona/status `dirty` oznacza, że bieżąca generacja lokalnego `SeenStore` nie została jeszcze potwierdzona jako wysłana; bezpośrednio po uruchomieniu może być `true` do zakończenia startup Sync nawet bez nowo obejrzanych klatek.

Źródła: [SeenStore](../random-frame/src-tauri/src/persistence.rs), [inicjalizacja aplikacji](../random-frame/src-tauri/src/lib.rs), [silnik Sync](../random-frame/src-tauri/src/sync.rs), [start UI](../random-frame/src/client/app.ts).

## Tożsamość, recovery key i szyfrowanie

Przy **Enable Sync** klient losuje 32-bajtowy sekret główny. Wyświetlany `rf1-…` recovery key jest jego przenośną reprezentacją z checksumą. Z sekretu klient wyprowadza osobne: klucz szyfrowania, bearer token i `sync_id`. Recovery key nie idzie do serwera. Na urządzeniu sekret jest zapisywany w systemowym magazynie poświadczeń (na testowanym Linux: Secret Service); lokalny `sync-config.json` zawiera `sync_id` i ostatnią zaakceptowaną rewizję, ale nie sekret.

Snapshot v1 to posortowane, unikatowe identyfikatory w binarnym formacie `RFSEEN`, maksymalnie 2 000 000 wpisów. Klient szyfruje go XChaCha20-Poly1305 z losowym nonce i uwierzytelnia razem z nagłówkiem oraz `sync_id`. Serwer widzi tylko binarny envelope, jego rozmiar, `sync_id`, rewizję i czasy operacji. W bazie zamiast bearera przechowuje `SHA-256` tokenu. Weryfikacja hasha używa porównania constant-time.

Recovery key trzeba traktować jak hasło dające dostęp do danych Sync: jego posiadacz może odtworzyć klucze i sparować urządzenie. Zapisz go poza aplikacją w bezpiecznym miejscu. Jeśli zaginie i żadne urządzenie nie ma już działającego sekretu, zaszyfrowany snapshot na VPS nie wystarczy do odzyskania danych. **Leave Sync** usuwa lokalny sekret i konfigurację, lecz nie usuwa chaina z serwera — API nie ma DELETE. Jeżeli recovery key nadal istnieje, Join może ponownie uzyskać dostęp do tego chaina.

Źródła: [format snapshotu](../random-frame/src-tauri/src/snapshot.rs), [kryptografia](../random-frame/src-tauri/src/sync_crypto.rs), [secure storage](../random-frame/src-tauri/src/secure_storage.rs).

## Przepływ Create, Join i Sync now

| Operacja | Co robi klient | Co robi serwer |
|---|---|---|
| Enable/Create | Generuje sekret, snapshot lokalnego `seen`, szyfruje go; `PUT` z `If-None-Match: *`. Po sukcesie zapisuje sekret i parowanie oraz pokazuje recovery key. | Tworzy nowy chain atomowo; zwraca `201`, `ETag: "1"`. |
| Join | Z recovery key odtwarza klucze, pobiera i uwierzytelnia envelope, odszyfrowuje go, scala ze swoim lokalnym `seen`, wysyła wynik, dopiero potem utrwala parowanie. | Zwraca envelope i rewizję; przy zapisie wykonuje CAS. |
| Sync now / startup Sync | Pobiera envelope, sprawdza rewizję i AEAD, scala z lokalnym zbiorem, szyfruje nowy snapshot, wysyła go z `If-Match`. | Podmienia bajty wyłącznie, jeśli rewizja nadal jest zgodna; zwiększa ją o 1. |

Przykład konfliktu: A i B pobierają rewizję 5. A wysyła `If-Match: "5"` i dostaje rewizję 6. B wysyła tę samą precondition i dostaje `412`. B pobiera rewizję 6, odszyfrowuje, scala ją ze swoimi ID, szyfruje nowy wynik i ponawia PUT. Klient ma limit trzech prób CAS. Rewizja może wzrosnąć również przy Sync bez nowych ID, bo obecna implementacja wysyła snapshot po GET.

Urządzenie pamięta ostatnią zaakceptowaną rewizję. Jeśli serwer zwróci mniejszą, klient zgłasza rollback zamiast scalić dane. To chroni sparowane urządzenie przed cofnięciem stanu serwera, lecz pierwsze Join nie ma jeszcze lokalnej „podłogi” rewizji i nie wykryje poprawnego kryptograficznie historycznego envelope podanego przed pierwszym parowaniem.

Źródła: [silnik Sync](../random-frame/src-tauri/src/sync.rs), [HTTP transport](../random-frame/src-tauri/src/sync_transport.rs), [API serwera](src/main.rs).

## HTTP API i odpowiedzialność serwera

| Żądanie | Znaczenie | Typowa odpowiedź |
|---|---|---|
| `GET /health` | Sprawdza również `SELECT 1` w SQLite. | `200 {"status":"ok"}` lub `500`. |
| `PUT /sync/{sync_id}` + `If-None-Match: *` | Utworzenie chaina z binarnym envelope. | `201`, `ETag: "1"`; istniejący ID: `412`. |
| `GET /sync/{sync_id}` | Pobranie dokładnie zapisanych bajtów. | `200` + `ETag`; brak ID lub zły bearer: jednakowe `404`. |
| `PUT /sync/{sync_id}` + `If-Match: "n"` | Atomowy update CAS. | `204`, `ETag: "n+1"`; stara rewizja: `412`; zły bearer: `404`. |

`sync_id` i bearer mają po 64 małe znaki hex. PUT wymaga `application/octet-stream`, niepustego body i limitu `16 000 068 B`. Błędny format to `400`, zbyt duży body `413`, błędy wewnętrzne DB `500`. Serwer nie parsuje formatu envelope ani nie sprawdza, czy da się go odszyfrować. Klient mapuje m.in. `429` na `rate_limited`, `5xx` na `server_error`, problem TLS na `tls`. W UI problem serwera nie blokuje lokalnego Draw.

SQLite ma jedną tabelę `sync_chains` (`sync_id`, `auth_verifier`, `revision`, `payload`, `created_at`, `updated_at`). Startup tworzy tabelę, jeśli jej nie ma; katalog rodzic DB musi już istnieć. Ustawienia połączeń: WAL, `synchronous=FULL`, busy timeout 5 s, maksymalnie 5 połączeń. Log aplikacji zawiera metodę, szablon trasy, status, czas i rodzaj błędu DB, ale nie nagłówki, bearer, pełny `sync_id` ani payload.

Źródła: [serwer](src/main.rs), [schemat](migrations/001_initial.sql), [transport klienta](../random-frame/src-tauri/src/sync_transport.rs).

## Faktyczna konfiguracja VPS

| Element | Stan odczytany 24.09.2026 |
|---|---|
| Host | Oracle VPS `150.230.159.236`, Ubuntu 24.04.4 LTS, x86_64; 954 MiB RAM, około 495 MiB dostępne, 41 GiB wolne na `/`. |
| Usługa | `random-frame-sync.service` aktywna i enabled, `User=Group=rf-sync`, bez sudo/login shell. |
| Binarka | `/opt/random-frame-sync/random-frame-sync-server`, `root:root 0755`. |
| Env | `/etc/random-frame-sync/server.env`, `root:rf-sync 0640`: `RF_SYNC_BIND=127.0.0.1:8787`, `RF_SYNC_DB=/var/lib/random-frame-sync/sync.db`, `RF_SYNC_MAX_PAYLOAD=16000068`, `RUST_LOG=info`. Nie ma prywatnego sekretu serwera. |
| Dane | `/var/lib/random-frame-sync/` `rf-sync:rf-sync 0700`; `sync.db` `rf-sync:rf-sync 0600`. |
| Ochrona systemd | `UMask=0077`, `NoNewPrivileges`, `PrivateTmp`, `ProtectSystem=strict`, `ProtectHome`, zapis tylko do katalogu DB. |
| Listener | Axum tylko `127.0.0.1:8787`; nginx na publicznym `443`. Publiczny `:8787` był niedostępny w teście wdrożeniowym. |
| Baza przy odczycie | `journal_mode=wal`, `integrity_check=ok`, 4 chainy, najwyższa rewizja 8. Są w niej również chainy testowe; API nie ma DELETE. |

Pliki na VPS: `/etc/systemd/system/random-frame-sync.service`, `/etc/nginx/sites-available/server-random-frame.amokrzycki.ovh`, `/etc/nginx/conf.d/random-frame-sync-limit.conf`. Odpowiadające im pliki repo są w [deploy/](deploy/); plik nginx w repo jest **bootstrapem HTTP**, a na VPS Certbot dodał do niego listener HTTPS i przekierowanie HTTP → HTTPS. Nie należy bezmyślnie nadpisywać pliku VPS wersją bootstrapową.

Nginx przyjmuje maksymalnie `17m` (mały zapas ponad limit Axum i klienta), proxy’uje do `http://127.0.0.1:8787`, wyłącza cache dla `/sync/` i access log dla całego vhosta. `limit_req_zone` to 30 żądań/min/IP, burst 10, odpowiedź 429. Prawdziwy adres klienta bierze z `CF-Connecting-IP` **tylko** dla wpisanych sieci Cloudflare; listę sieci trzeba okresowo porównywać z aktualną publikacją Cloudflare. Firewall VPS ma regułę dopuszczającą TCP 443. Inne vhosty VPS, w tym `wap.amokrzycki.ovh`, pozostają niezależne.

DNS `server-random-frame.amokrzycki.ovh` wskazywał podczas kontroli na `188.114.96.0` i `188.114.97.0` (adresy proxy Cloudflare), nie bezpośrednio na VPS. Publiczny i bezpośredni-origin `GET /health` zwróciły HTTP 200 z poprawną walidacją TLS (`curl` bez `-k`). Certyfikat Certbota obejmuje dokładnie ten hostname i wygasa **22.12.2026 o 19:56:51 UTC**. `certbot.timer` jest aktywny; wcześniejszy `certbot renew --dry-run` przeszedł. Cloudflare nie było konfigurowane z tego środowiska: trybu SSL/TLS nie dało się odczytać z panelu. Docelowo należy potwierdzić **Full (strict)**. Udany HTTPS origin i przekierowanie HTTP → HTTPS są testem działania ścieżki, ale nie odczytem tej opcji panelu.

## Backup i odtwarzanie

`random-frame-sync-backup.timer` jest enabled i aktywny; uruchamia codziennie oneshot `random-frame-sync-backup.service`, z losowym opóźnieniem do 1 h i `Persistent=true`. Skrypt `/opt/random-frame-sync/backup.py` używa `sqlite3.Connection.backup()`, więc tworzy spójną kopię podczas pracy w WAL. Nadaje nazwę z timestampem UTC, uruchamia `PRAGMA integrity_check`, usuwa lokalne kopie starsze niż 14 dni. Katalog `/var/backups/random-frame-sync/` to `root:root 0700`, pliki `0600`. W kontroli były dwie poprawnie utworzone kopie. Są **na tym samym VPS**: nie chronią przed utratą całej maszyny. Nie ma skonfigurowanego backupu off-site.

Nie kopiuj samego aktywnego `sync.db` zwykłym `cp`: najnowsze transakcje mogą być w WAL. Przy odtwarzaniu najpierw zabezpiecz aktualny stan, zatrzymaj usługę i zweryfikuj wybraną kopię; dopiero potem przeprowadź kontrolowany restore z właściwymi owner/perms i ponownie uruchom usługę. Restore starszej kopii może obniżyć rewizję, a wtedy urządzenia, które zaakceptowały wyższą rewizję, zgłoszą rollback. To wymaga osobnej decyzji operacyjnej, a nie automatycznego „naprawienia” klienta.

Na VPS **nie ma** CLI `sqlite3`; baza i backup korzystają z biblioteki SQLite. Do odczytowej diagnostyki można użyć dostępnego `python3` z modułem `sqlite3`.

## Konfiguracja klienta i artefakty

W [lib.rs](../random-frame/src-tauri/src/lib.rs) URL jest pobierany najpierw z runtime `RANDOM_FRAME_SYNC_BASE_URL`, a gdy go brak — z wartości `option_env!` osadzonej podczas builda. Produkcyjne AppImage i deb zbudowano z `https://server-random-frame.amokrzycki.ovh`. Przyszły build musi ponownie dostać tę zmienną, jeśli ma mieć URL bez ręcznego runtime env. Transport pozwala na HTTP **tylko** dla `localhost`/adresu loopback; publiczny endpoint musi być HTTPS.

| Artefakt | Co rzeczywiście sprawdzono |
|---|---|
| Development `tauri build --no-bundle` | Build przeszedł; to nie jest test instalatora. |
| AppImage | Uruchomienie GUI, Sync, prawdziwy Secret Service, restart i startup Sync, offline/recovery, Join/Leave oraz interakcje dialogu przeszły w izolowanej sesji testowej. |
| deb | Pakiet zbudowano, **nie instalowano** go na Debianie/Ubuntu; secure storage w tym pakiecie pozostaje niezweryfikowany. |
| Windows | Brak testu na rzeczywistym Windows/Credential Manager. |

## Jak szybko sprawdzić stan bez zmieniania danych

Do SSH używaj IP i zwykłego OpenSSH; proxied hostname Cloudflare nie jest endpointem SSH. Na tym komputerze potrzebne było `-F /dev/null`, aby ominąć wadliwe uprawnienia lokalnego pliku globalnej konfiguracji SSH:

```sh
ssh -F /dev/null -i ~/.ssh/new-vm ubuntu@150.230.159.236
```

Na VPS:

```sh
sudo systemctl status random-frame-sync random-frame-sync-backup.timer certbot.timer
sudo journalctl -u random-frame-sync -n 100 --no-pager
sudo ss -lntp | grep -E '(:8787|:443)'
curl --fail http://127.0.0.1:8787/health
sudo nginx -t
sudo certbot certificates
sudo systemctl list-timers random-frame-sync-backup.timer certbot.timer
sudo ls -lh /var/backups/random-frame-sync/
```

Z komputera klienta:

```sh
dig +short server-random-frame.amokrzycki.ovh
curl --fail --show-error https://server-random-frame.amokrzycki.ovh/health
curl --fail --show-error --resolve server-random-frame.amokrzycki.ovh:443:150.230.159.236 \
  https://server-random-frame.amokrzycki.ovh/health
```

Żaden z tych `curl` nie używa `-k`. Nie wkładaj prawdziwego recovery key ani bearera do historii powłoki. Nie używaj `curl -v` z prawdziwym tokenem, bo wypisze nagłówek `Authorization`.

## Co test produkcyjny faktycznie udowodnił

Osobne profile A i B, przez realny HTTPS i z losowym testowym recovery key, przeszły Create → Join → rozbieżne lokalne ID → merge/CAS → identyczne lokalne zbiory i odszyfrowany zdalny snapshot. Rewizje szły **1 → 2 → 3 → 4 → 5**, po restarcie usługi do **6**, po zatrzymaniu backendu i lokalnym dodaniu ID urządzenie pokazało `dirty=true` oraz błąd serwera, a po wznowieniu doszło do **7 → 8**. Test nie mockował transportu. Oddzielny smoke API sprawdził 201/ETag 1, identyczny GET, 204/ETag 2, stale 412 i wrong-bearer 404. Kontrolowany smoke nginx potwierdził 429 bez agresywnego floodowania.

To **nie** dowodzi jeszcze działania instalacji deb na docelowej dystrybucji, Windows Credential Manager ani ustawienia Full (strict) odczytanego w panelu Cloudflare. Są to pozostałe release gates. Nie należy na tej podstawie oznaczać wszystkich wariantów jako w pełni release-ready.
