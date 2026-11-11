-- #4 Escalation nativo — stato persistente delle rotation (semantica GoAlert rotation_state).
-- position = indice membro attivo; shift_start = inizio del turno corrente (griglia allineata a start).
-- Semina: alla prima osservazione shadow si copia l'indice corrente da GoAlert (ground truth),
-- poi l'engine nativo avanza indipendente di +1 a ogni boundary (calcAdvance-like).
CREATE TABLE IF NOT EXISTS app.esc_rotation_state (
    tenant_id   uuid        NOT NULL,
    group_id    uuid        NOT NULL,
    position    int         NOT NULL DEFAULT 0,
    shift_start timestamptz NOT NULL,
    seeded_from text        NOT NULL DEFAULT 'goalert',  -- 'goalert' (shadow) | 'native' (cutover)
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, group_id)
);
