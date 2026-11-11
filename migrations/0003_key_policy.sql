CREATE FUNCTION gw_check_key_policy_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF OLD.key_id IS NOT NULL AND NEW IS DISTINCT FROM OLD THEN
  RAISE EXCEPTION 'immutable issued key policy' USING ERRCODE='23514';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER gw_key_policy_immutable BEFORE UPDATE ON gw_policies FOR EACH ROW EXECUTE FUNCTION gw_check_key_policy_immutable();
