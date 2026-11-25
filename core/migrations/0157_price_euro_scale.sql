-- CA-6 (seguito di 0156): il prezzo in euro calcolato dai centesimi con 2 decimali esatti
-- (0156 lo produceva con 16 decimali: 42.5000000000000000).
ALTER TABLE app.price_list_items DROP COLUMN unit_price;
ALTER TABLE app.price_list_items ADD COLUMN unit_price numeric(14,2) GENERATED ALWAYS AS ((price_cents / 100.0)::numeric(14,2)) STORED;
ALTER TABLE app.work_order_lines DROP COLUMN unit_price;
ALTER TABLE app.work_order_lines ADD COLUMN unit_price numeric(14,2) GENERATED ALWAYS AS ((unit_price_cents / 100.0)::numeric(14,2)) STORED;
